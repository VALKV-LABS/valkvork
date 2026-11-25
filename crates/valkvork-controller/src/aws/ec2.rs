//! The `aws-ebs` volume driver (docs/block-volumes.md, stage 3).
//!
//! Volumes are tagged `valkvork:volume=<name>` and `valkvork:cell=<cell>`,
//! encrypted, and created with a client token, so a repeated create (a
//! controller restart mid-create) finds or makes the same volume. Attach and
//! detach return once EC2 reports them done.

use super::client::{AwsClient, AwsError};
use super::xml;
use crate::volumes::{
    CloudVolume, CloudVolumeState, DriverError, InstanceState, Result, VolumeDriver,
};
use async_trait::async_trait;
use std::time::Duration;

pub const TAG_VOLUME: &str = "valkvork:volume";
pub const TAG_CELL: &str = "valkvork:cell";
const GIB: u64 = 1 << 30;

pub struct EbsDriver {
    client: AwsClient,
    cell: String,
    /// Between state checks, and the most to wait for one transition.
    poll: Duration,
    wait: Duration,
}

fn err(e: AwsError) -> DriverError {
    DriverError(e.to_string())
}

fn p(k: impl Into<String>, v: impl Into<String>) -> (String, String) {
    (k.into(), v.into())
}

/// One `<item>` of a `volumeSet`.
fn parse_volume(item: &str) -> CloudVolume {
    let base = xml::without(&xml::without(item, "attachmentSet"), "tagSet");
    let attached_to = xml::inner(item, "attachmentSet")
        .map(|set| xml::elements(set, "item"))
        .unwrap_or_default()
        .into_iter()
        .find(|a| {
            matches!(
                xml::text(a, "status").as_deref(),
                Some("attaching" | "attached" | "detaching")
            )
        })
        .and_then(|a| xml::text(a, "instanceId"));
    CloudVolume {
        cloud_id: xml::text(&base, "volumeId").unwrap_or_default(),
        zone: xml::text(&base, "availabilityZone").unwrap_or_default(),
        size_bytes: xml::text(&base, "size")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
            * GIB,
        state: match xml::text(&base, "status").as_deref() {
            Some("creating") => CloudVolumeState::Creating,
            Some("available") => CloudVolumeState::Available,
            Some("in-use") => CloudVolumeState::InUse,
            Some("deleting") | Some("deleted") => CloudVolumeState::Deleting,
            _ => CloudVolumeState::Error,
        },
        attached_to,
    }
}

/// The attachment state of `item` for `instance`, if any.
fn attachment_status(item: &str, instance: &str) -> Option<String> {
    xml::elements(xml::inner(item, "attachmentSet")?, "item")
        .into_iter()
        .find(|a| xml::text(a, "instanceId").as_deref() == Some(instance))
        .and_then(|a| xml::text(a, "status"))
}

impl EbsDriver {
    pub fn new(client: AwsClient, cell: String) -> Self {
        Self {
            client,
            cell,
            poll: Duration::from_secs(2),
            wait: Duration::from_secs(180),
        }
    }

    #[cfg(test)]
    fn fast(mut self) -> Self {
        self.poll = Duration::from_millis(5);
        self.wait = Duration::from_secs(5);
        self
    }

    async fn describe_items(&self, params: Vec<(String, String)>) -> Result<Vec<String>> {
        let body = self
            .client
            .ec2("DescribeVolumes", &params)
            .await
            .map_err(err)?;
        let set = xml::inner(&body, "volumeSet").unwrap_or("");
        Ok(xml::elements(set, "item")
            .into_iter()
            .map(str::to_string)
            .collect())
    }

    async fn describe_raw(&self, cloud_id: &str) -> Result<String> {
        match self.describe_items(vec![p("VolumeId.1", cloud_id)]).await {
            Ok(items) => items
                .into_iter()
                .next()
                .ok_or_else(|| DriverError(format!("volume {cloud_id} not found"))),
            Err(e) if e.0.starts_with("InvalidVolume.NotFound") => {
                Err(DriverError(format!("volume {cloud_id} not found")))
            }
            Err(e) => Err(e),
        }
    }

    /// The live volume this cell created for `name`, if any.
    async fn find(&self, name: &str) -> Result<Option<CloudVolume>> {
        let items = self
            .describe_items(vec![
                p("Filter.1.Name", format!("tag:{TAG_VOLUME}")),
                p("Filter.1.Value.1", name),
                p("Filter.2.Name", format!("tag:{TAG_CELL}")),
                p("Filter.2.Value.1", &self.cell),
            ])
            .await?;
        Ok(items
            .iter()
            .map(|i| parse_volume(i))
            .find(|v| v.state != CloudVolumeState::Deleting))
    }

    /// Poll `cloud_id` until `done` holds for it.
    async fn wait_for(
        &self,
        cloud_id: &str,
        what: &str,
        done: impl Fn(&str) -> bool,
    ) -> Result<String> {
        let deadline = tokio::time::Instant::now() + self.wait;
        loop {
            let item = self.describe_raw(cloud_id).await?;
            if done(&item) {
                return Ok(item);
            }
            if parse_volume(&item).state == CloudVolumeState::Error {
                return Err(DriverError(format!(
                    "volume {cloud_id} is in the error state"
                )));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(DriverError(format!(
                    "volume {cloud_id}: not {what} after {} s",
                    self.wait.as_secs()
                )));
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// A device name the instance does not use yet (`/dev/sdf` … `/dev/sdz`):
    /// EC2 needs one, though Nitro instances show the volume by its id.
    async fn free_device(&self, instance_id: &str) -> Result<String> {
        let body = self
            .client
            .ec2("DescribeInstances", &[p("InstanceId.1", instance_id)])
            .await
            .map_err(err)?;
        let used: Vec<String> = xml::inner(&body, "blockDeviceMapping")
            .map(|m| xml::elements(m, "item"))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| xml::text(i, "deviceName"))
            .collect();
        ('f'..='z')
            .map(|c| format!("/dev/sd{c}"))
            .find(|d| {
                !used
                    .iter()
                    .any(|u| u == d || *u == d.replace("/dev/sd", "/dev/xvd"))
            })
            .ok_or_else(|| DriverError(format!("instance {instance_id} has no free device name")))
    }
}

#[async_trait]
impl VolumeDriver for EbsDriver {
    fn name(&self) -> &'static str {
        "aws-ebs"
    }

    async fn create(
        &self,
        name: &str,
        size_bytes: u64,
        class: &str,
        zone: &str,
    ) -> Result<CloudVolume> {
        let id = match self.find(name).await? {
            Some(v) => v.cloud_id,
            None => {
                let gib = size_bytes.div_ceil(GIB).max(1);
                let token: String = format!("vk-{}-{name}", self.cell)
                    .chars()
                    .take(64)
                    .collect();
                let body = self
                    .client
                    .ec2(
                        "CreateVolume",
                        &[
                            p("AvailabilityZone", zone),
                            p("Size", gib.to_string()),
                            p("VolumeType", class),
                            p("Encrypted", "true"),
                            p("ClientToken", token),
                            p("TagSpecification.1.ResourceType", "volume"),
                            p("TagSpecification.1.Tag.1.Key", TAG_VOLUME),
                            p("TagSpecification.1.Tag.1.Value", name),
                            p("TagSpecification.1.Tag.2.Key", TAG_CELL),
                            p("TagSpecification.1.Tag.2.Value", &self.cell),
                            p("TagSpecification.1.Tag.3.Key", "Name"),
                            p(
                                "TagSpecification.1.Tag.3.Value",
                                format!("valkvork-{}-{name}", self.cell),
                            ),
                        ],
                    )
                    .await
                    .map_err(err)?;
                xml::text(&body, "volumeId")
                    .ok_or_else(|| DriverError("CreateVolume returned no volumeId".into()))?
            }
        };
        let item = self
            .wait_for(&id, "available", |i| {
                matches!(
                    parse_volume(i).state,
                    CloudVolumeState::Available | CloudVolumeState::InUse
                )
            })
            .await?;
        Ok(parse_volume(&item))
    }

    async fn describe(&self, cloud_id: &str) -> Result<CloudVolume> {
        Ok(parse_volume(&self.describe_raw(cloud_id).await?))
    }

    async fn attach(&self, cloud_id: &str, instance_id: &str) -> Result<()> {
        let v = self.describe(cloud_id).await?;
        match v.attached_to.as_deref() {
            Some(i) if i == instance_id => {}
            Some(other) => {
                return Err(DriverError(format!(
                    "volume {cloud_id} is attached to {other}"
                )));
            }
            None => {
                let device = self.free_device(instance_id).await?;
                self.client
                    .ec2(
                        "AttachVolume",
                        &[
                            p("VolumeId", cloud_id),
                            p("InstanceId", instance_id),
                            p("Device", device),
                        ],
                    )
                    .await
                    .map_err(err)?;
            }
        }
        self.wait_for(cloud_id, "attached", |i| {
            attachment_status(i, instance_id).as_deref() == Some("attached")
        })
        .await?;
        Ok(())
    }

    async fn detach(&self, cloud_id: &str, instance_id: &str, force: bool) -> Result<()> {
        let v = self.describe(cloud_id).await?;
        let Some(holder) = v.attached_to else {
            return Ok(());
        };
        if holder != instance_id && !force {
            return Err(DriverError(format!(
                "volume {cloud_id} is attached to {holder}, not {instance_id}"
            )));
        }
        let mut params = vec![p("VolumeId", cloud_id), p("InstanceId", &holder)];
        if force {
            params.push(p("Force", "true"));
        }
        match self.client.ec2("DetachVolume", &params).await {
            Ok(_) => {}
            // Detached meanwhile.
            Err(e) if e.code == "IncorrectState" => {}
            Err(e) => return Err(err(e)),
        }
        self.wait_for(cloud_id, "detached", |i| {
            parse_volume(i).attached_to.is_none()
        })
        .await?;
        Ok(())
    }

    async fn delete(&self, cloud_id: &str) -> Result<()> {
        match self
            .client
            .ec2("DeleteVolume", &[p("VolumeId", cloud_id)])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.code == "InvalidVolume.NotFound" => Ok(()),
            Err(e) => Err(err(e)),
        }
    }

    async fn instance_state(&self, instance_id: &str) -> Result<InstanceState> {
        let body = match self
            .client
            .ec2("DescribeInstances", &[p("InstanceId.1", instance_id)])
            .await
        {
            Ok(b) => b,
            Err(e) if e.code == "InvalidInstanceID.NotFound" => return Ok(InstanceState::NotFound),
            Err(e) => return Err(err(e)),
        };
        let state = xml::inner(&body, "instanceState").and_then(|s| xml::text(s, "name"));
        Ok(match state.as_deref() {
            Some("pending") => InstanceState::Pending,
            Some("running") => InstanceState::Running,
            Some("stopping") => InstanceState::Stopping,
            Some("stopped") => InstanceState::Stopped,
            Some("shutting-down") => InstanceState::ShuttingDown,
            Some("terminated") => InstanceState::Terminated,
            _ => InstanceState::NotFound,
        })
    }
}

#[cfg(test)]
mod tests {
    //! The driver against a fake EC2 endpoint that keeps volumes in memory
    //! and answers the Query API's XML.
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// (zone, GiB, status, (instance, attachment status), tags)
    type FakeVolume = (
        String,
        u64,
        String,
        Option<(String, String)>,
        Vec<(String, String)>,
    );

    #[derive(Default)]
    struct Fake {
        volumes: HashMap<String, FakeVolume>,
        instances: HashMap<String, String>,
        next: u32,
        calls: Vec<String>,
    }

    fn form(body: &str) -> HashMap<String, String> {
        body.split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (dec(k), dec(v)))
            .collect()
    }

    fn dec(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn volume_xml(id: &str, v: &FakeVolume) -> String {
        let (zone, gib, status, att, tags) = v;
        let att = att
            .as_ref()
            .map(|(i, s)| format!("<item><volumeId>{id}</volumeId><instanceId>{i}</instanceId><device>/dev/sdf</device><status>{s}</status></item>"))
            .unwrap_or_default();
        let tags: String = tags
            .iter()
            .map(|(k, v)| format!("<item><key>{k}</key><value>{v}</value></item>"))
            .collect();
        format!("<item><volumeId>{id}</volumeId><size>{gib}</size><availabilityZone>{zone}</availabilityZone><status>{status}</status><attachmentSet>{att}</attachmentSet><tagSet>{tags}</tagSet></item>")
    }

    fn handle(f: &mut Fake, body: &str) -> (u16, String) {
        let q = form(body);
        let action = q["Action"].clone();
        f.calls.push(action.clone());
        let e = |code: &str| {
            (400, format!("<Response><Errors><Error><Code>{code}</Code><Message>m</Message></Error></Errors></Response>"))
        };
        match action.as_str() {
            "CreateVolume" => {
                f.next += 1;
                let id = format!("vol-{:04}", f.next);
                let tags = (1..=3)
                    .filter_map(|n| {
                        Some((
                            q.get(&format!("TagSpecification.1.Tag.{n}.Key"))?.clone(),
                            q.get(&format!("TagSpecification.1.Tag.{n}.Value"))?.clone(),
                        ))
                    })
                    .collect();
                assert_eq!(q["Encrypted"], "true");
                f.volumes.insert(
                    id.clone(),
                    (
                        q["AvailabilityZone"].clone(),
                        q["Size"].parse().unwrap(),
                        "available".into(),
                        None,
                        tags,
                    ),
                );
                (200, format!("<CreateVolumeResponse><volumeId>{id}</volumeId><status>creating</status></CreateVolumeResponse>"))
            }
            "DescribeVolumes" => {
                let items: String = if let Some(id) = q.get("VolumeId.1") {
                    match f.volumes.get(id) {
                        Some(v) => volume_xml(id, v),
                        None => return e("InvalidVolume.NotFound"),
                    }
                } else {
                    let want_name = q.get("Filter.1.Value.1").cloned().unwrap_or_default();
                    let want_cell = q.get("Filter.2.Value.1").cloned().unwrap_or_default();
                    f.volumes
                        .iter()
                        .filter(|(_, v)| {
                            v.4.contains(&(TAG_VOLUME.into(), want_name.clone()))
                                && v.4.contains(&(TAG_CELL.into(), want_cell.clone()))
                        })
                        .map(|(id, v)| volume_xml(id, v))
                        .collect()
                };
                (200, format!("<DescribeVolumesResponse><volumeSet>{items}</volumeSet></DescribeVolumesResponse>"))
            }
            "AttachVolume" => {
                let v = f.volumes.get_mut(&q["VolumeId"]).unwrap();
                assert!(q["Device"].starts_with("/dev/sd"));
                v.3 = Some((q["InstanceId"].clone(), "attached".into()));
                v.2 = "in-use".into();
                (
                    200,
                    "<AttachVolumeResponse><status>attaching</status></AttachVolumeResponse>"
                        .into(),
                )
            }
            "DetachVolume" => {
                let v = f.volumes.get_mut(&q["VolumeId"]).unwrap();
                v.3 = None;
                v.2 = "available".into();
                (200, "<DetachVolumeResponse/>".into())
            }
            "DeleteVolume" => match f.volumes.remove(&q["VolumeId"]) {
                Some(_) => (200, "<DeleteVolumeResponse/>".into()),
                None => e("InvalidVolume.NotFound"),
            },
            "DescribeInstances" => {
                let id = &q["InstanceId.1"];
                match f.instances.get(id) {
                    Some(state) => (200, format!(
                        "<DescribeInstancesResponse><reservationSet><item><instancesSet><item><instanceId>{id}</instanceId><instanceState><code>16</code><name>{state}</name></instanceState><blockDeviceMapping><item><deviceName>/dev/sda1</deviceName></item><item><deviceName>/dev/sdf</deviceName></item></blockDeviceMapping></item></instancesSet></item></reservationSet></DescribeInstancesResponse>"
                    )),
                    None => e("InvalidInstanceID.NotFound"),
                }
            }
            _ => e("InvalidAction"),
        }
    }

    async fn fake_ec2() -> (String, Arc<Mutex<Fake>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let fake = Arc::new(Mutex::new(Fake::default()));
        let f = fake.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let f = f.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    // Read headers, then the body by Content-Length.
                    let (head_end, len) = loop {
                        let n = s.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                            assert!(
                                head.contains("authorization: aws4-hmac-sha256 credential=akid/"),
                                "{head}"
                            );
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .map(|v| v.trim().parse::<usize>().unwrap())
                                .unwrap_or(0);
                            break (i + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = s.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                    let (status, text) = handle(&mut f.lock().unwrap(), &body);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (url, fake)
    }

    async fn driver() -> (EbsDriver, Arc<Mutex<Fake>>) {
        let (url, fake) = fake_ec2().await;
        let client = AwsClient::new(Some("us-west-1".into()), Some(url))
            .await
            .unwrap();
        client
            .set_credentials(super::super::sigv4::Credentials {
                access_key_id: "AKID".into(),
                secret_access_key: "secret".into(),
                session_token: Some("tok".into()),
            })
            .await;
        (EbsDriver::new(client, "test-cell".into()).fast(), fake)
    }

    #[tokio::test]
    async fn create_is_idempotent_and_tagged() {
        let (d, fake) = driver().await;
        let v = d
            .create("orders-db", 10 * GIB, "gp3", "us-west-1a")
            .await
            .unwrap();
        assert_eq!(
            (v.zone.as_str(), v.size_bytes, v.state),
            ("us-west-1a", 10 * GIB, CloudVolumeState::Available)
        );
        let again = d
            .create("orders-db", 10 * GIB, "gp3", "us-west-1a")
            .await
            .unwrap();
        assert_eq!(
            again.cloud_id, v.cloud_id,
            "found by its tags, not created twice"
        );
        let f = fake.lock().unwrap();
        assert_eq!(f.calls.iter().filter(|c| *c == "CreateVolume").count(), 1);
        assert!(f.volumes[&v.cloud_id]
            .4
            .contains(&(TAG_CELL.into(), "test-cell".into())));
    }

    #[tokio::test]
    async fn attach_detach_fence_and_delete() {
        let (d, fake) = driver().await;
        fake.lock()
            .unwrap()
            .instances
            .insert("i-a".into(), "running".into());
        fake.lock()
            .unwrap()
            .instances
            .insert("i-b".into(), "stopped".into());
        let v = d.create("db", GIB, "gp3", "us-west-1a").await.unwrap();
        d.attach(&v.cloud_id, "i-a").await.unwrap();
        d.attach(&v.cloud_id, "i-a").await.unwrap();
        assert_eq!(
            d.describe(&v.cloud_id)
                .await
                .unwrap()
                .attached_to
                .as_deref(),
            Some("i-a")
        );
        assert!(
            d.attach(&v.cloud_id, "i-b").await.is_err(),
            "one attachment"
        );
        assert!(d.detach(&v.cloud_id, "i-b", false).await.is_err());
        // Fencing: force-detach from whoever holds it.
        let v = d.create("db2", GIB, "gp3", "us-west-1a").await.unwrap();
        d.attach(&v.cloud_id, "i-a").await.unwrap();
        d.detach(&v.cloud_id, "i-b", true).await.unwrap();
        assert_eq!(d.describe(&v.cloud_id).await.unwrap().attached_to, None);
        d.detach(&v.cloud_id, "i-a", false).await.unwrap();
        assert_eq!(
            d.instance_state("i-a").await.unwrap(),
            InstanceState::Running
        );
        assert!(d.instance_state("i-b").await.unwrap().is_fenced());
        assert_eq!(
            d.instance_state("i-gone").await.unwrap(),
            InstanceState::NotFound
        );
        d.delete(&v.cloud_id).await.unwrap();
        d.delete(&v.cloud_id).await.unwrap();
        assert!(d.describe(&v.cloud_id).await.is_err());
    }
}

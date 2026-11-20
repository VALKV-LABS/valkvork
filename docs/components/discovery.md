# Component: Service Discovery

## High-Level Function

A workload finds another service by name: `global-cp.svc.valkvork.internal` resolves to the addresses of the nodes where global-cp is running and healthy. The controller works out those addresses and sends them to every agent. Each agent answers DNS for them on its own node and forwards every other query to the node's normal resolvers. It is off unless agent.toml has `[discovery] enabled = true`.

The code is in `crates/valkvork-controller/src/core/discovery.rs` (the set and when it is sent) and `crates/valkvork-agent/src/discovery.rs` (the store and the DNS server).

```
controller ── Endpoints {global-cp: [10.12.0.11, 10.12.0.12], ...} ──► every agent
                                                                         │ saves it
container ── "global-cp.svc.valkvork.internal A?" ──► agent DNS on 172.17.0.1:53
          ◄── 10.12.0.12, 10.12.0.11 (rotated, TTL 5 s)
container ── "registry-1.docker.io A?" ──► agent ──► the node's resolvers (/etc/resolv.conf)
```

## Which Addresses

A service's set holds the node address (`Register.ip_address`, the advertise address) of each workload that is:

- RUNNING, with desired state RUN,
- on a HEALTHY node,
- HEALTHY, if the service has a `[check]`. Without one, RUNNING is enough.

Every long-running service is listed, even with no addresses, so its name exists and has no records (`NOERROR`, empty), while an unknown name is `NXDOMAIN`. Batch jobs are not listed. The client connects to the address and the service's published `host_port`: ports are static (FR-NET-03), so the port comes from the manifest, not from DNS.

A workload whose port binds `127.0.0.1` is still listed, but other nodes can't reach that port. Discovery is for ports published on the node's address (`host_ip = "advertise"` or every address).

## Sending the Set

After every step, the controller computes the set and compares it with the last one it sent. The whole set is sent:

- to every connected agent when it changed,
- to an agent after it registers, right after its `RegisterAck`.

Only agents that list the `service_discovery` feature get it. An older agent wouldn't recognise the message. If an agent's stream is busy, the controller sends to it again at the next step. **Nothing is sent during the recovery grace period**: health is not reported again yet after a controller restart, and an empty or partial set would make agents drop addresses that still work. The agents keep the set they have.

The message is `Endpoints { version, services: [{service_id, addresses}] }`. `version` only increases while one controller process runs, and agents don't compare versions: the latest message wins.

## The Agent

The agent keeps the last set in memory and in `<data_dir>/endpoints.json`, so after a restart it can answer before it reaches the controller, or while it can't.

With `[discovery] enabled = true`, at startup it:

1. picks the address: `discovery.listen_ip`, or by default the Docker `bridge` network's gateway (usually `172.17.0.1`);
2. takes the upstream resolvers from `discovery.upstream`, or the `nameserver` lines of `/etc/resolv.conf` (for example systemd-resolved's `127.0.0.53`), leaving out its own address;
3. listens on UDP and TCP port 53 there, and **exits if it can't**: containers that expect service names but can't resolve them are worse than an agent that fails to start, which systemd retries and the journal shows;
4. creates every container with that address as its only nameserver (Docker `--dns`).

Containers that already exist keep their DNS settings until they are replaced. After you turn discovery on, run an update, or let them be replaced over time.

### Answers

| Query | Answer |
|---|---|
| `A` / `AAAA` / `ANY` for `<service>.<domain>`, a known service | Its IPv4 / IPv6 addresses, rotated on each query, authoritative, TTL `ttl_seconds` (5). Over UDP only as many as fit in 512 bytes, about 28. Over TCP, all of them |
| other types for a known service | `NOERROR`, no records |
| an unknown service, or a deeper name such as `a.b.<domain>` | `NXDOMAIN` |
| `<domain>` itself | `NOERROR`, no records |
| any other name | Forwarded unchanged to each upstream in turn (2 s each), over UDP or TCP as it came. `SERVFAIL` if none answers |
| unparseable | `FORMERR`. A response packet is never answered |

Names are matched case-insensitively. The agent writes no SOA record, so resolvers don't cache negative answers. The DNS handling is written in the agent, with no extra dependency, and serves only A and AAAA records.

### Why One Nameserver, and Why on the Bridge

Containers get only the agent as nameserver. With a second one, musl (Alpine images) sends to both and takes the first reply, which could be the upstream's `NXDOMAIN` for a service name. So containers depend on the agent for every lookup. An agent restart takes about a second, and resolvers retry.

The agent opens no listener on the network (FR-ARC-01). The bridge gateway is an address on the node that only the node and its containers can reach. Other machines in the VPC can't query it, so it is neither an open resolver nor a map of the cell. Processes on the node itself can query it too: `dig @172.17.0.1 global-cp.svc.valkvork.internal`.

## Configuration

```toml
[discovery]
enabled     = true
domain      = "svc.valkvork.internal"   # names are <service>.<domain>
# listen_ip = "172.17.0.1"              # default: the Docker bridge gateway
# upstream  = ["10.12.0.2:53"]          # default: /etc/resolv.conf's nameservers
ttl_seconds = 5
```

`--check-config` rejects a `domain` that is not a DNS name, and a `listen_ip` of `0.0.0.0`, `::` or a multicast address.

## On the Node

The host firewall must let containers reach port 53 on the bridge gateway. With UFW's default input policy `DROP`, add for example `ufw allow in on docker0 to any port 53`. Without it, containers time out on every lookup.

## Not Built

- SRV records with ports. Ports are static today. With dynamic ports (FR-NET-03, [phase-2.md](../phase-2.md#dynamic-ports-and-load-balancer-registration)), the port must travel with the address.
- Choosing nearby addresses, such as the client's own node first. Answers are rotated, not ordered.
- Per-workload addresses (container IPs) or names for single workloads.

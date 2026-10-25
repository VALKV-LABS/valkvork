//! Manifest validation for Valkvork.
//!
//! This module implements the validation rules from spec §6.2 for TOML manifests.

use std::collections::HashSet;
use regex::Regex;
use crate::models::{ServiceSpec, AutoscaleConfig, PortMapping, VolumeMount};
use crate::errors::{ValidationError, ValidationErrorCode};

// ============================================================================
// Validation Functions
// ============================================================================

/// Validate a service specification
pub fn validate_service_spec(spec: &ServiceSpec) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    
    // VE-01: Resource validation
    validate_resources(&mut errors, spec);
    
    // VE-02: Image reference validation
    validate_image(&mut errors, &spec.image);
    
    // VE-03: Service ID validation
    validate_service_id(&mut errors, &spec.id);
    
    // VE-04: Autoscale validation
    if let Some(autoscale) = &spec.autoscale {
        validate_autoscale(&mut errors, autoscale);
    }
    
    // VE-05: Port validation
    validate_ports(&mut errors, &spec.ports);
    
    // VE-06: Unknown fields - this is handled by TOML parser with deny_unknown_fields
    
    // VE-07: Volume validation
    validate_volumes(&mut errors, &spec.volumes);
    
    errors
}

/// Validate resource specifications (VE-01)
fn validate_resources(errors: &mut Vec<ValidationError>, spec: &ServiceSpec) {
    // CPU must be > 0
    if spec.cpu_millicores == 0 {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidResourceSpec,
            "resources.cpu",
            "CPU must be greater than 0"
        ));
    }
    
    // Memory must be >= 4 MiB (4 * 1024 * 1024 bytes)
    if spec.memory_bytes < 4 * 1024 * 1024 {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidResourceSpec,
            "resources.memory",
            "Memory must be at least 4 MiB"
        ));
    }
    
    // Disk must be > 0
    if spec.disk_bytes == 0 {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidResourceSpec,
            "resources.disk",
            "Disk must be greater than 0"
        ));
    }
    
    // Replicas must be >= 0
    if spec.replicas < 0 {
        // Note: replicas is u32, so this can't happen, but for completeness
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidResourceSpec,
            "service.replicas",
            "Replicas must be >= 0"
        ));
    }
}

/// Validate image reference (VE-02)
fn validate_image(errors: &mut Vec<ValidationError>, image: &str) {
    // Valid OCI reference: [domain/]repo[:tag|@digest]
    // This is a simplified validation
    
    let valid_pattern = Regex::new(r"^[a-z0-9\-\.]+(/[a-z0-9\-\._]+)*(:[a-zA-Z0-9\-\._]+|@sha256:[a-f0-9]{64})?$").unwrap();
    
    if !valid_pattern.is_match(image) {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidImageRef,
            "service.image",
            &format!("Invalid OCI reference: {}", image)
        ));
    }
    
    // Check if tag is missing (implies :latest)
    if !image.contains(':') && !image.contains('@') {
        errors.push(ValidationError::warning(
            ValidationErrorCode::InvalidImageRef,
            "service.image",
            "No tag specified, defaults to :latest. Consider pinning to a specific tag or digest."
        ));
    }
}

/// Validate service ID (VE-03)
/// Must match: ^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$
fn validate_service_id(errors: &mut Vec<ValidationError>, id: &str) {
    let valid_pattern = Regex::new(r"^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$").unwrap();
    
    if !valid_pattern.is_match(id) {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidServiceId,
            "service.id",
            &format!(
                "Invalid service ID: {}. Must match: ^[a-z0-9]([-a-z0-9]{{0,61}}[a-z0-9])?$",
                id
            )
        ));
    }
}

/// Validate autoscale configuration (VE-04)
fn validate_autoscale(errors: &mut Vec<ValidationError>, autoscale: &AutoscaleConfig) {
    // 1 <= min_replicas <= max_replicas
    if autoscale.min_replicas < 1 {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidAutoscaleSpec,
            "autoscale.min_replicas",
            "min_replicas must be >= 1"
        ));
    }
    
    if autoscale.max_replicas < autoscale.min_replicas {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidAutoscaleSpec,
            "autoscale.max_replicas",
            "max_replicas must be >= min_replicas"
        ));
    }
    
    // 0 < target_cpu_percent < 100
    if autoscale.target_cpu_percent <= 0.0 || autoscale.target_cpu_percent >= 100.0 {
        errors.push(ValidationError::new(
            ValidationErrorCode::InvalidAutoscaleSpec,
            "autoscale.target_cpu_percent",
            "target_cpu_percent must be > 0 and < 100"
        ));
    }
}

/// Validate port mappings (VE-05)
fn validate_ports(errors: &mut Vec<ValidationError>, ports: &[PortMapping]) {
    let mut seen_ports = HashSet::new();
    
    for port in ports {
        // Port numbers must be in 1-65535
        if port.container_port == 0 || port.container_port > 65535 {
            errors.push(ValidationError::new(
                ValidationErrorCode::InvalidPortSpec,
                "ports.container_port",
                &format!(
                    "Container port {} must be between 1 and 65535",
                    port.container_port
                )
            ));
        }
        
        if port.host_port == 0 || port.host_port > 65535 {
            errors.push(ValidationError::new(
                ValidationErrorCode::InvalidPortSpec,
                "ports.host_port",
                &format!(
                    "Host port {} must be between 1 and 65535",
                    port.host_port
                )
            ));
        }
        
        // Host ports must be unique within the manifest
        if !seen_ports.insert(port.host_port) {
            errors.push(ValidationError::new(
                ValidationErrorCode::InvalidPortSpec,
                "ports.host_port",
                &format!("Duplicate host port: {}", port.host_port)
            ));
        }
        
        // Warn if host_port < 1024
        if port.host_port < 1024 {
            errors.push(ValidationError::warning(
                ValidationErrorCode::InvalidPortSpec,
                "ports.host_port",
                &format!("Host port {} is < 1024. Consider using a higher port.", port.host_port)
            ));
        }
    }
}

/// Validate volume mounts (VE-07)
fn validate_volumes(errors: &mut Vec<ValidationError>, volumes: &[VolumeMount]) {
    let mut seen_targets = HashSet::new();
    
    for volume in volumes {
        // Type must be known (MVP: only "host" is valid)
        if volume.r#type != "host" {
            errors.push(ValidationError::new(
                ValidationErrorCode::InvalidVolumeSpec,
                "volumes.type",
                &format!("Unknown volume type: {}. MVP only supports 'host'.", volume.r#type)
            ));
        }
        
        // Target must be absolute path
        if !volume.target.starts_with('/') {
            errors.push(ValidationError::new(
                ValidationErrorCode::InvalidVolumeSpec,
                "volumes.target",
                &format!("Volume target '{}' must be an absolute path", volume.target)
            ));
        }
        
        // Source must be non-empty
        if volume.source.is_empty() {
            errors.push(ValidationError::new(
                ValidationErrorCode::InvalidVolumeSpec,
                "volumes.source",
                "Volume source must be specified"
            ));
        }
        
        // Targets must be unique
        if !seen_targets.insert(&volume.target) {
            errors.push(ValidationError::new(
                ValidationErrorCode::InvalidVolumeSpec,
                "volumes.target",
                &format!("Duplicate volume target: {}", volume.target)
            ));
        }
    }
}

// ============================================================================
// Manifest Parsing
// ============================================================================

use toml;
use std::path::Path;
use std::fs;

/// Parse a TOML manifest file into a ServiceSpec
pub fn parse_manifest(path: &Path) -> Result<ServiceSpec, Vec<ValidationError>> {
    let content = fs::read_to_string(path).map_err(|e| {
        vec![ValidationError::new(
            ValidationErrorCode::UnknownField,
            "",
            &format!("Failed to read manifest: {}", e)
        )]
    })?;
    
    parse_manifest_str(&content)
}

/// Parse a TOML manifest string into a ServiceSpec
pub fn parse_manifest_str(content: &str) -> Result<ServiceSpec, Vec<ValidationError>> {
    let value: toml::Value = toml::from_str(content).map_err(|e| {
        vec![ValidationError::new(
            ValidationErrorCode::UnknownField,
            "",
            &format!("TOML parse error: {}", e)
        )]
    })?;
    
    // Convert TOML value to ServiceSpec
    let spec = from_toml_value(&value).map_err(|e| {
        vec![ValidationError::new(
            ValidationErrorCode::UnknownField,
            "",
            &format!("Manifest conversion error: {}", e)
        )]
    })?;
    
    // Validate the spec
    let errors = validate_service_spec(&spec);
    
    if errors.iter().any(|e| !e.is_warning) {
        Err(errors)
    } else {
        Ok(spec)
    }
}

/// Convert TOML value to ServiceSpec
fn from_toml_value(value: &toml::Value) -> Result<ServiceSpec, String> {
    let mut spec = ServiceSpec::default();
    
    // Extract service table
    let service_table = value.get("service")
        .ok_or("Missing [service] table")?
        .as_table()
        .ok_or("service must be a table")?;
    
    spec.id = service_table.get("id")
        .ok_or("Missing service.id")?
        .as_str()
        .ok_or("service.id must be a string")?
        .to_string();
    
    spec.image = service_table.get("image")
        .ok_or("Missing service.image")?
        .as_str()
        .ok_or("service.image must be a string")?
        .to_string();
    
    spec.replicas = service_table.get("replicas")
        .map(|v| v.as_integer().unwrap_or(1) as u32)
        .unwrap_or(1);
    
    spec.kill_timeout_seconds = service_table.get("kill_timeout")
        .map(|v| parse_duration_from_toml(v))
        .unwrap_or(10);
    
    // Extract resources table
    if let Some(resources_table) = value.get("resources").and_then(|v| v.as_table()) {
        spec.cpu_millicores = resources_table.get("cpu")
            .map(|v| (v.as_float().unwrap_or(0.0) * 1000.0).ceil() as u32)
            .unwrap_or(100);
        
        spec.memory_bytes = resources_table.get("memory")
            .map(|v| parse_size_from_toml(v))
            .unwrap_or(128 * 1024 * 1024);
        
        spec.disk_bytes = resources_table.get("disk")
            .map(|v| parse_size_from_toml(v))
            .unwrap_or(1 * 1024 * 1024 * 1024);
    }
    
    // Extract constraints table
    if let Some(constraints_table) = value.get("constraints").and_then(|v| v.as_table()) {
        spec.required_class = constraints_table.get("required_class")
            .map(|v| v.as_str().unwrap_or("").to_string());
    }
    
    // Extract env table
    if let Some(env_table) = value.get("env").and_then(|v| v.as_table()) {
        for (key, value) in env_table {
            spec.env.insert(key.to_string(), value.as_str().unwrap_or("").to_string());
        }
    }
    
    // Extract ports array
    if let Some(ports_array) = value.get("ports").and_then(|v| v.as_array()) {
        for port_value in ports_array {
            if let Some(port_table) = port_value.as_table() {
                let port = PortMapping {
                    name: port_table.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    container_port: port_table.get("container_port").and_then(|v| v.as_integer()).unwrap_or(0) as u16,
                    host_port: port_table.get("host_port").and_then(|v| v.as_integer()).unwrap_or(0) as u16,
                };
                spec.ports.push(port);
            }
        }
    }
    
    // Extract volumes array
    if let Some(volumes_array) = value.get("volumes").and_then(|v| v.as_array()) {
        for volume_value in volumes_array {
            if let Some(volume_table) = volume_value.as_table() {
                let volume = VolumeMount {
                    r#type: volume_table.get("type").and_then(|v| v.as_str()).unwrap_or("host").to_string(),
                    source: volume_table.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    target: volume_table.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    read_only: volume_table.get("read_only").and_then(|v| v.as_bool()).unwrap_or(false),
                };
                spec.volumes.push(volume);
            }
        }
    }
    
    // Extract autoscale table
    if let Some(autoscale_table) = value.get("autoscale").and_then(|v| v.as_table()) {
        spec.autoscale = Some(AutoscaleConfig {
            min_replicas: autoscale_table.get("min_replicas").and_then(|v| v.as_integer()).unwrap_or(1) as u32,
            max_replicas: autoscale_table.get("max_replicas").and_then(|v| v.as_integer()).unwrap_or(10) as u32,
            target_cpu_percent: autoscale_table.get("target_cpu_percent").and_then(|v| v.as_float()).unwrap_or(75.0),
        });
    }
    
    // Extract update table
    if let Some(update_table) = value.get("update").and_then(|v| v.as_table()) {
        spec.update.max_parallel = update_table.get("max_parallel").and_then(|v| v.as_integer()).unwrap_or(1) as u32;
        spec.update.min_healthy_time_seconds = update_table.get("min_healthy_time")
            .map(|v| parse_duration_from_toml(v))
            .unwrap_or(10);
    }
    
    // Extract restart table
    if let Some(restart_table) = value.get("restart").and_then(|v| v.as_table()) {
        spec.restart.attempts = restart_table.get("attempts").and_then(|v| v.as_integer()).unwrap_or(3) as u32;
        spec.restart.interval_seconds = restart_table.get("interval")
            .map(|v| parse_duration_from_toml(v))
            .unwrap_or(300);
        spec.restart.delay_seconds = restart_table.get("delay")
            .map(|v| parse_duration_from_toml(v))
            .unwrap_or(5);
    }
    
    Ok(spec)
}

/// Parse duration from TOML value
fn parse_duration_from_toml(value: &toml::Value) -> u64 {
    if let Some(s) = value.as_str() {
        crate::units::parse_duration(s).unwrap_or(0)
    } else if let Some(i) = value.as_integer() {
        i as u64
    } else {
        0
    }
}

/// Parse size from TOML value
fn parse_size_from_toml(value: &toml::Value) -> u64 {
    if let Some(s) = value.as_str() {
        crate::units::parse_size(s).unwrap_or(0).as_u64()
    } else if let Some(i) = value.as_integer() {
        i as u64
    } else {
        0
    }
}

/// Serialize ServiceSpec to TOML string
pub fn spec_to_toml(spec: &ServiceSpec) -> String {
    let mut table = toml::value::Table::new();
    
    // Service table
    let mut service_table = toml::value::Table::new();
    service_table.insert("id".to_string(), toml::Value::String(spec.id.clone()));
    service_table.insert("image".to_string(), toml::Value::String(spec.image.clone()));
    service_table.insert("replicas".to_string(), toml::Value::Integer(spec.replicas as i64));
    service_table.insert("kill_timeout".to_string(), toml::Value::String(format!("{}s", spec.kill_timeout_seconds)));
    table.insert("service".to_string(), toml::Value::Table(service_table));
    
    // Resources table
    let mut resources_table = toml::value::Table::new();
    resources_table.insert("cpu".to_string(), toml::Value::Float(spec.cpu_millicores as f64 / 1000.0));
    resources_table.insert("memory".to_string(), toml::Value::String(format!("{}MiB", spec.memory_bytes / (1024 * 1024))));
    resources_table.insert("disk".to_string(), toml::Value::String(format!("{}GiB", spec.disk_bytes / (1024 * 1024 * 1024))));
    table.insert("resources".to_string(), toml::Value::Table(resources_table));
    
    // Constraints table
    if let Some(class) = &spec.required_class {
        let mut constraints_table = toml::value::Table::new();
        constraints_table.insert("required_class".to_string(), toml::Value::String(class.clone()));
        table.insert("constraints".to_string(), toml::Value::Table(constraints_table));
    }
    
    // Env table
    if !spec.env.is_empty() {
        let mut env_table = toml::value::Table::new();
        for (key, value) in &spec.env {
            env_table.insert(key.clone(), toml::Value::String(value.clone()));
        }
        table.insert("env".to_string(), toml::Value::Table(env_table));
    }
    
    // Ports array
    if !spec.ports.is_empty() {
        let ports: Vec<toml::Value> = spec.ports.iter().map(|port| {
            let mut port_table = toml::value::Table::new();
            port_table.insert("name".to_string(), toml::Value::String(port.name.clone()));
            port_table.insert("container_port".to_string(), toml::Value::Integer(port.container_port as i64));
            port_table.insert("host_port".to_string(), toml::Value::Integer(port.host_port as i64));
            toml::Value::Table(port_table)
        }).collect();
        table.insert("ports".to_string(), toml::Value::Array(ports));
    }
    
    // Volumes array
    if !spec.volumes.is_empty() {
        let volumes: Vec<toml::Value> = spec.volumes.iter().map(|volume| {
            let mut volume_table = toml::value::Table::new();
            volume_table.insert("type".to_string(), toml::Value::String(volume.r#type.clone()));
            volume_table.insert("source".to_string(), toml::Value::String(volume.source.clone()));
            volume_table.insert("target".to_string(), toml::Value::String(volume.target.clone()));
            volume_table.insert("read_only".to_string(), toml::Value::Boolean(volume.read_only));
            toml::Value::Table(volume_table)
        }).collect();
        table.insert("volumes".to_string(), toml::Value::Array(volumes));
    }
    
    // Autoscale table
    if let Some(autoscale) = &spec.autoscale {
        let mut autoscale_table = toml::value::Table::new();
        autoscale_table.insert("min_replicas".to_string(), toml::Value::Integer(autoscale.min_replicas as i64));
        autoscale_table.insert("max_replicas".to_string(), toml::Value::Integer(autoscale.max_replicas as i64));
        autoscale_table.insert("target_cpu_percent".to_string(), toml::Value::Float(autoscale.target_cpu_percent));
        table.insert("autoscale".to_string(), toml::Value::Table(autoscale_table));
    }
    
    // Update table
    let mut update_table = toml::value::Table::new();
    update_table.insert("max_parallel".to_string(), toml::Value::Integer(spec.update.max_parallel as i64));
    update_table.insert("min_healthy_time".to_string(), toml::Value::String(format!("{}s", spec.update.min_healthy_time_seconds)));
    table.insert("update".to_string(), toml::Value::Table(update_table));
    
    // Restart table
    let mut restart_table = toml::value::Table::new();
    restart_table.insert("attempts".to_string(), toml::Value::Integer(spec.restart.attempts as i64));
    restart_table.insert("interval".to_string(), toml::Value::String(format!("{}s", spec.restart.interval_seconds)));
    restart_table.insert("delay".to_string(), toml::Value::String(format!("{}s", spec.restart.delay_seconds)));
    table.insert("restart".to_string(), toml::Value::Table(restart_table));
    
    toml::to_string(&toml::Value::Table(table)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_valid_manifest() {
        let toml = r#"
[service]
id = "test-service"
image = "nginx:latest"
replicas = 3

[resources]
cpu = 0.5
memory = "512MiB"

[constraints]
required_class = "general"

[[ports]]
name = "http"
container_port = 80
host_port = 8080

[env]
ENV_VAR = "value"
        "#;
        
        let result = parse_manifest_str(toml);
        assert!(result.is_ok());
        let spec = result.unwrap();
        
        assert_eq!(spec.id, "test-service");
        assert_eq!(spec.image, "nginx:latest");
        assert_eq!(spec.replicas, 3);
        assert_eq!(spec.cpu_millicores, 500);
        assert_eq!(spec.memory_bytes, 512 * 1024 * 1024);
        assert_eq!(spec.required_class, Some("general".to_string()));
        assert_eq!(spec.ports.len(), 1);
        assert_eq!(spec.ports[0].name, "http");
        assert_eq!(spec.ports[0].container_port, 80);
        assert_eq!(spec.ports[0].host_port, 8080);
        assert_eq!(spec.env.get("ENV_VAR"), Some(&"value".to_string()));
    }
    
    #[test]
    fn test_validate_service_id() {
        let mut spec = ServiceSpec::default();
        spec.id = "invalid!id".to_string();
        
        let errors = validate_service_spec(&spec);
        assert!(!errors.is_empty());
        assert_eq!(errors[0].code, ValidationErrorCode::InvalidServiceId);
    }
}

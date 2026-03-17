use std::net::Ipv6Addr;
use std::process::Command;
use std::sync::OnceLock;

use rand::Rng;
use tracing::{debug, info, warn};

struct Ipv6Block {
    prefix: u128,
    prefix_len: u8,
}

static BLOCK: OnceLock<Ipv6Block> = OnceLock::new();

pub fn random_source_addr() -> Option<String> {
    let block = BLOCK.get()?;
    Some(block.random_addr().to_string())
}

impl Ipv6Block {
    fn random_addr(&self) -> Ipv6Addr {
        let host_bits = 128u32 - self.prefix_len as u32;
        let host_mask = if host_bits >= 128 {
            u128::MAX
        } else {
            (1u128 << host_bits) - 1
        };
        let prefix_part = self.prefix & !host_mask;

        let mut rng = rand::thread_rng();
        loop {
            let random_host = rng.gen::<u128>() & host_mask;
            if random_host != 0 && random_host != host_mask {
                return Ipv6Addr::from(prefix_part | random_host);
            }
        }
    }
}

pub fn detect_and_setup() -> anyhow::Result<()> {
    let output = Command::new("ip")
        .args(["-6", "addr", "show", "scope", "global"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run `ip`: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (addr, prefix_len, _interface) = parse_ipv6_global(&stdout)
        .ok_or_else(|| anyhow::anyhow!("no global-scope IPv6 address found on any interface"))?;

    let host_bits = 128u32 - prefix_len as u32;
    let host_mask = if host_bits >= 128 {
        u128::MAX
    } else {
        (1u128 << host_bits) - 1
    };
    let prefix = u128::from(addr) & !host_mask;
    let prefix_addr = Ipv6Addr::from(prefix);
    let route_target = format!("{prefix_addr}/{prefix_len}");

    info!(
        "ASTRALINK_IPV6_DETECTED addr={} prefix={} prefix_len={}",
        addr, route_target, prefix_len
    );

    if prefix_len > 96 {
        warn!(
            "ASTRALINK_IPV6_SMALL_BLOCK prefix_len={} (only {} addresses available)",
            prefix_len,
            1u128 << host_bits
        );
    }

    match Command::new("sysctl")
        .args(["-w", "net.ipv6.ip_nonlocal_bind=1"])
        .output()
    {
        Ok(out) if out.status.success() => {
            info!("ASTRALINK_IPV6_NONLOCAL_BIND_ENABLED");
        }
        Ok(out) => {
            debug!(
                "ASTRALINK_IPV6_NONLOCAL_BIND_SKIPPED stderr={}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            debug!("ASTRALINK_IPV6_SYSCTL_CMD_UNAVAILABLE error={}", e);
        }
    }

    let route_result = Command::new("ip")
        .args([
            "-6",
            "route",
            "replace",
            "local",
            &route_target,
            "dev",
            "lo",
        ])
        .output();

    match route_result {
        Ok(out) if out.status.success() => {
            info!("ASTRALINK_IPV6_ROUTE_SET target={}", route_target);
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!(
                "ASTRALINK_IPV6_ROUTE_FAILED target={} stderr={}",
                route_target,
                stderr.trim()
            );
        }
        Err(e) => {
            warn!("ASTRALINK_IPV6_ROUTE_CMD_FAILED error={}", e);
        }
    }

    let block = Ipv6Block { prefix, prefix_len };
    let test_addr = block.random_addr();
    info!("ASTRALINK_IPV6_READY test_addr={}", test_addr);

    BLOCK
        .set(block)
        .map_err(|_| anyhow::anyhow!("IPv6 block already initialized"))?;

    Ok(())
}

fn parse_ipv6_global(output: &str) -> Option<(Ipv6Addr, u8, String)> {
    let mut current_interface = String::new();

    for line in output.lines() {
        let trimmed = line.trim();

        if let Some(ch) = trimmed.chars().next() {
            if ch.is_ascii_digit() {
                if let Some(iface) = trimmed.split(':').nth(1) {
                    current_interface = iface.trim().to_string();
                }
            }
        }

        if trimmed.starts_with("inet6 ") && trimmed.contains("scope global") {
            let addr_part = trimmed.split_whitespace().nth(1)?;
            let (addr_str, prefix_str) = addr_part.split_once('/')?;
            let addr: Ipv6Addr = addr_str.parse().ok()?;
            let prefix_len: u8 = prefix_str.parse().ok()?;

            if prefix_len < 128 {
                return Some((addr, prefix_len, current_interface));
            }
        }
    }

    None
}

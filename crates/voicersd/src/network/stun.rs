use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::{
    net::{lookup_host, UdpSocket},
    sync::RwLock,
    time::timeout,
};
use voicers_core::DaemonStatus;

use std::sync::Arc;

use super::{insert_unique_addr, insert_unique_note, push_note};

const STUN_TIMEOUT: Duration = Duration::from_secs(3);
const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_BINDING_SUCCESS_RESPONSE: u16 = 0x0101;
const STUN_ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const STUN_ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;

pub(super) async fn run_stun_probes(state: Arc<RwLock<DaemonStatus>>, servers: Vec<String>) {
    for server in servers {
        match probe_stun_server(&server).await {
            Ok(mapped_addr) => {
                let rendered = format!("stun:{server} -> {mapped_addr}");
                let mut state = state.write().await;
                insert_unique_addr(&mut state.network.stun_addrs, rendered.clone());
                state.network.nat_status = format!("STUN reflexive UDP endpoint {mapped_addr}");
                insert_unique_note(&mut state, format!("STUN observed {rendered}"));
            }
            Err(error) => {
                push_note(
                    &state,
                    format!("STUN probe failed against {server}: {error:#}"),
                )
                .await;
            }
        }
    }
}

async fn probe_stun_server(server: &str) -> Result<SocketAddr> {
    let mut resolved = lookup_host(server)
        .await
        .with_context(|| format!("failed to resolve STUN server {server}"))?;
    let server_addr = resolved
        .next()
        .with_context(|| format!("STUN server {server} did not resolve to an address"))?;
    let bind_addr = if server_addr.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    };
    let socket = UdpSocket::bind(bind_addr)
        .await
        .context("failed to bind UDP socket for STUN probe")?;

    let transaction_id = stun_transaction_id();
    let request = stun_binding_request(transaction_id);
    socket
        .send_to(&request, server_addr)
        .await
        .with_context(|| format!("failed to send STUN request to {server_addr}"))?;

    let mut response = [0u8; 1500];
    let (len, _) = timeout(STUN_TIMEOUT, socket.recv_from(&mut response))
        .await
        .with_context(|| format!("timed out waiting for STUN response from {server_addr}"))?
        .context("failed to receive STUN response")?;

    parse_stun_binding_response(&response[..len], transaction_id)
}

fn stun_transaction_id() -> [u8; 12] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let pid = u128::from(std::process::id());
    let mixed = nanos ^ pid.rotate_left(17);
    let bytes = mixed.to_be_bytes();
    let mut transaction_id = [0u8; 12];
    transaction_id.copy_from_slice(&bytes[4..16]);
    transaction_id
}

fn stun_binding_request(transaction_id: [u8; 12]) -> [u8; 20] {
    let mut request = [0u8; 20];
    request[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
    request[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    request[8..20].copy_from_slice(&transaction_id);
    request
}

fn parse_stun_binding_response(response: &[u8], transaction_id: [u8; 12]) -> Result<SocketAddr> {
    if response.len() < 20 {
        anyhow::bail!("STUN response is shorter than the header");
    }

    let message_type = u16::from_be_bytes([response[0], response[1]]);
    if message_type != STUN_BINDING_SUCCESS_RESPONSE {
        anyhow::bail!("unexpected STUN response type 0x{message_type:04x}");
    }

    let message_length = usize::from(u16::from_be_bytes([response[2], response[3]]));
    if response.len() < 20 + message_length {
        anyhow::bail!("truncated STUN response");
    }

    let magic_cookie = u32::from_be_bytes([response[4], response[5], response[6], response[7]]);
    if magic_cookie != STUN_MAGIC_COOKIE {
        anyhow::bail!("unexpected STUN magic cookie");
    }

    if response[8..20] != transaction_id {
        anyhow::bail!("STUN transaction id mismatch");
    }

    let attrs = &response[20..20 + message_length];
    let mut mapped_addr = None;
    let mut offset = 0usize;
    while offset + 4 <= attrs.len() {
        let attr_type = u16::from_be_bytes([attrs[offset], attrs[offset + 1]]);
        let attr_len = usize::from(u16::from_be_bytes([attrs[offset + 2], attrs[offset + 3]]));
        let value_start = offset + 4;
        let value_end = value_start + attr_len;
        if value_end > attrs.len() {
            anyhow::bail!("truncated STUN attribute");
        }

        let value = &attrs[value_start..value_end];
        match attr_type {
            STUN_ATTR_XOR_MAPPED_ADDRESS => {
                return parse_stun_address(value, true, transaction_id);
            }
            STUN_ATTR_MAPPED_ADDRESS => {
                mapped_addr = Some(parse_stun_address(value, false, transaction_id)?);
            }
            _ => {}
        }

        offset = value_start + padded_stun_attr_len(attr_len);
    }

    mapped_addr.context("STUN response did not include a mapped address")
}

fn parse_stun_address(value: &[u8], xor: bool, transaction_id: [u8; 12]) -> Result<SocketAddr> {
    if value.len() < 4 || value[0] != 0 {
        anyhow::bail!("invalid STUN address attribute");
    }

    let family = value[1];
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    if xor {
        port ^= (STUN_MAGIC_COOKIE >> 16) as u16;
    }

    match family {
        0x01 => {
            if value.len() < 8 {
                anyhow::bail!("truncated STUN IPv4 address");
            }
            let mut octets = [value[4], value[5], value[6], value[7]];
            if xor {
                let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
                for (octet, mask) in octets.iter_mut().zip(cookie) {
                    *octet ^= mask;
                }
            }
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        0x02 => {
            if value.len() < 20 {
                anyhow::bail!("truncated STUN IPv6 address");
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            if xor {
                let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
                for (octet, mask) in octets.iter_mut().take(4).zip(cookie) {
                    *octet ^= mask;
                }
                for (octet, mask) in octets.iter_mut().skip(4).zip(transaction_id) {
                    *octet ^= mask;
                }
            }
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        other => anyhow::bail!("unsupported STUN address family 0x{other:02x}"),
    }
}

fn padded_stun_attr_len(len: usize) -> usize {
    (len + 3) & !3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_ipv4_stun_response() {
        let transaction_id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let public_ip = Ipv4Addr::new(203, 0, 113, 45);
        let public_port = 54_321u16;
        let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
        let xport = public_port ^ (STUN_MAGIC_COOKIE >> 16) as u16;
        let mut xaddr = public_ip.octets();
        for (octet, mask) in xaddr.iter_mut().zip(cookie) {
            *octet ^= mask;
        }

        let mut response = Vec::new();
        response.extend_from_slice(&STUN_BINDING_SUCCESS_RESPONSE.to_be_bytes());
        response.extend_from_slice(&12u16.to_be_bytes());
        response.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        response.extend_from_slice(&transaction_id);
        response.extend_from_slice(&STUN_ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        response.extend_from_slice(&8u16.to_be_bytes());
        response.push(0);
        response.push(0x01);
        response.extend_from_slice(&xport.to_be_bytes());
        response.extend_from_slice(&xaddr);

        let parsed = parse_stun_binding_response(&response, transaction_id).unwrap();
        assert_eq!(parsed, SocketAddr::new(IpAddr::V4(public_ip), public_port));
    }
}

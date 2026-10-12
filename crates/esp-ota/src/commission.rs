//! Enrollment: commissioning a device into the esp-ota fabric.
//!
//! The standard Matter sequence over a PASE session (ArmFailSafe, attestation,
//! CSR, AddTrustedRootCertificate, AddNOC), optional Wi-Fi provisioning, then
//! CASE and CommissioningComplete. Existing fabrics such as Apple Home are
//! untouched.

use std::net::SocketAddr;

use anyhow::{anyhow, Context, Result};
use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN;
use rs_matter::cert::MAX_CERT_TLV_LEN;
use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::network_commissioning::{
    NetworkCommissioningClient, NetworkCommissioningStatusEnum,
};
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::onboard::{CommissionOptions, CommissionResult, Commissioner};
use rs_matter::tlv::Octets;
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::Address;

use crate::controller::{peer, secret_key, Controller};

pub struct WifiCredentials {
    pub ssid: String,
    pub password: String,
}

/// Commission over `addr` with the setup `passcode`, assigning `node_id`.
///
/// `operational` resolves the device's address after AddNOC; over IP it is
/// the same address, over Bluetooth it is found after the device joins Wi-Fi.
pub async fn commission<C, F, Fut>(
    ctl: &Controller<'_, C>,
    addr: Address,
    passcode: u32,
    node_id: u64,
    wifi: Option<&WifiCredentials>,
    operational: F,
) -> Result<SocketAddr>
where
    C: Crypto,
    F: FnOnce() -> Fut,
    Fut: core::future::Future<Output = Result<SocketAddr>>,
{
    let icac_key = secret_key(&ctl.fabric.icac_key)?;
    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut nocs = NocGenerator::new(ctl.matter, icac_key.reference(), ctl.fab_idx, &mut noc_buf)
        .map_err(|e| anyhow!("loading fabric CA: {e:?}"))?;
    let mut buf = [0u8; MAX_CERT_TLV_LEN];
    let mut commissioner =
        Commissioner::new(ctl.matter, &ctl.crypto, ctl.fab_idx, &mut nocs, &mut buf);
    let opts = CommissionOptions {
        // rs-matter 0.2 cannot verify attestation yet; our devices use test DACs.
        allow_test_attestation: true,
        fail_safe_secs: 120,
    };
    let result: CommissionResult = commissioner
        .commission(addr, passcode, &opts, node_id, VALID_FOREVER)
        .await
        .map_err(|e| anyhow!("commissioning (PASE, certificates): {e:?}"))?;

    if let Some(wifi) = wifi {
        provision_wifi(ctl, addr, passcode, wifi).await?;
    }

    let operational = operational().await?;
    commissioner
        .complete_via_case(Address::Udp(peer(operational)), &result)
        .await
        .map_err(|e| anyhow!("CASE and CommissioningComplete at {operational}: {e:?}"))?;
    Ok(operational)
}

/// AddOrUpdateWiFiNetwork then ConnectNetwork over the PASE session.
async fn provision_wifi<C: Crypto>(
    ctl: &Controller<'_, C>,
    addr: Address,
    passcode: u32,
    wifi: &WifiCredentials,
) -> Result<()> {
    let exchange = Exchange::initiate_pase(ctl.matter, &ctl.crypto, addr, passcode)
        .await
        .map_err(|e| anyhow!("reusing PASE session: {e:?}"))?;
    let handle = exchange
        .network_commissioning()
        .add_or_update_wi_fi_network(ROOT_ENDPOINT_ID, |req| {
            req.ssid(Octets(wifi.ssid.as_bytes()))?
                .credentials(Octets(wifi.password.as_bytes()))?
                .breadcrumb(Some(1))?
                .network_identity(None)?
                .client_identifier(None)?
                .possession_nonce(None)?
                .end()
        })
        .await
        .map_err(|e| anyhow!("AddOrUpdateWiFiNetwork: {e:?}"))?;
    let status = handle
        .response()
        .and_then(|r| r.networking_status())
        .map_err(|e| anyhow!("AddOrUpdateWiFiNetwork response: {e:?}"))?;
    handle.complete().await.map_err(|e| anyhow!("{e:?}"))?;
    if status != NetworkCommissioningStatusEnum::Success {
        anyhow::bail!("device rejected the Wi-Fi network: {status:?}");
    }

    let exchange = Exchange::initiate_pase(ctl.matter, &ctl.crypto, addr, passcode)
        .await
        .map_err(|e| anyhow!("reusing PASE session: {e:?}"))?;
    let handle = exchange
        .network_commissioning()
        .connect_network(ROOT_ENDPOINT_ID, |req| {
            req.network_id(Octets(wifi.ssid.as_bytes()))?
                .breadcrumb(Some(2))?
                .end()
        })
        .await
        .map_err(|e| anyhow!("ConnectNetwork: {e:?}"))?;
    let status = handle
        .response()
        .and_then(|r| r.networking_status())
        .map_err(|e| anyhow!("ConnectNetwork response: {e:?}"))?;
    handle.complete().await.map_err(|e| anyhow!("{e:?}"))?;
    if status != NetworkCommissioningStatusEnum::Success {
        anyhow::bail!("device could not join {:?}: {status:?}", wifi.ssid);
    }
    Ok(())
}

/// Parses an 11-digit manual pairing code into `(passcode, short discriminator)`.
pub fn parse_manual_code(code: &str) -> Result<(u32, u8)> {
    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 11 && digits.len() != 21 {
        anyhow::bail!("a manual pairing code has 11 digits");
    }
    let d = |range: core::ops::Range<usize>| -> Result<u32> {
        digits[range].parse::<u32>().context("bad digits")
    };
    let first = d(0..1)?;
    if first >> 2 != 0 && digits.len() == 11 {
        anyhow::bail!("unsupported pairing code version");
    }
    let middle = d(1..6)?;
    let last = d(6..10)?;
    let passcode = (middle & 0x3FFF) | (last << 14);
    let short_discriminator = (((first & 0x3) << 2) | (middle >> 14)) as u8;
    Ok((passcode, short_discriminator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rs_matter::BasicCommData;

    #[test]
    fn manual_code_round_trips_rs_matter_generation() {
        for (passcode, discriminator) in [(20202021u32, 3840u16), (12345678, 0x0abc), (1, 0x0fff)] {
            let comm = BasicCommData {
                password: passcode.to_le_bytes().into(),
                discriminator,
            };
            let code = comm.compute_pretty_pairing_code();
            let (parsed, short) = parse_manual_code(&code).unwrap();
            assert_eq!(parsed, passcode);
            assert_eq!(short, (discriminator >> 8) as u8);
        }
        assert!(parse_manual_code("1234").is_err());
    }
}

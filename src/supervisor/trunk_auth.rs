//! Version 3 authenticates both endpoints without sending the PSK and
//! requires the dialer's subnet before bringing the trunk up. Frames
//! remain plaintext and unauthenticated. Use a trusted network or a VPN.
use anyhow::{Result, ensure};
use hmac::{Hmac, Mac};
use ipnet::Ipv4Net;
use sha2_10::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(super) const MAGIC: &[u8; 12] = b"VMLABTRUNK3\n";
pub(super) const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn proof(
    psk: &str,
    role: &[u8],
    segment: &[u8],
    subnet: &[u8; 5],
    server: &[u8; 32],
    client: &[u8; 32],
) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(psk.as_bytes()).expect("HMAC accepts any key size");
    mac.update(MAGIC);
    mac.update(role);
    mac.update(&[segment.len() as u8]);
    mac.update(segment);
    mac.update(subnet);
    mac.update(server);
    mac.update(client);
    mac
}

pub(super) async fn client(
    stream: &mut TcpStream,
    segment: &str,
    subnet: Ipv4Net,
    psk: &str,
) -> Result<()> {
    ensure!(!psk.is_empty(), "empty trunk PSK");
    ensure!(
        !segment.is_empty() && segment.len() <= 128,
        "invalid trunk segment length"
    );
    let mut magic = [0; 12];
    stream.read_exact(&mut magic).await?;
    ensure!(&magic == MAGIC, "unsupported trunk protocol");
    let mut server = [0; 32];
    stream.read_exact(&mut server).await?;
    let client: [u8; 32] = rand::random();
    let [a, b, c, d] = subnet.addr().octets();
    let subnet = [a, b, c, d, subnet.prefix_len()];
    let name = segment.as_bytes();
    stream.write_u8(segment.len() as u8).await?;
    stream.write_all(name).await?;
    stream.write_all(&subnet).await?;
    stream.write_all(&client).await?;
    stream
        .write_all(
            &proof(psk, b"client", name, &subnet, &server, &client)
                .finalize()
                .into_bytes(),
        )
        .await?;
    let mut received = [0; 32];
    stream.read_exact(&mut received).await?;
    // verify_slice performs a constant-time MAC comparison.
    proof(psk, b"server", name, &subnet, &server, &client)
        .verify_slice(&received)
        .map_err(|_| anyhow::anyhow!("trunk server authentication failed"))?;
    Ok(())
}

pub(super) async fn server(stream: &mut TcpStream, psk: &str) -> Result<(String, Ipv4Net)> {
    ensure!(!psk.is_empty(), "empty trunk PSK");
    let server: [u8; 32] = rand::random();
    stream.write_all(MAGIC).await?;
    stream.write_all(&server).await?;
    let len = stream.read_u8().await? as usize;
    ensure!(len > 0 && len <= 128, "invalid trunk segment length");
    let mut segment = vec![0; len];
    stream.read_exact(&mut segment).await?;
    let segment = String::from_utf8(segment)?;
    let name = segment.as_bytes();
    let mut subnet = [0; 5];
    stream.read_exact(&mut subnet).await?;
    let mut client = [0; 32];
    let mut received = [0; 32];
    stream.read_exact(&mut client).await?;
    stream.read_exact(&mut received).await?;
    proof(psk, b"client", name, &subnet, &server, &client)
        .verify_slice(&received)
        .map_err(|_| anyhow::anyhow!("trunk client authentication failed"))?;
    let [a, b, c, d, prefix] = subnet;
    let network = Ipv4Net::new(std::net::Ipv4Addr::new(a, b, c, d), prefix)?;
    stream
        .write_all(
            &proof(psk, b"server", name, &subnet, &server, &client)
                .finalize()
                .into_bytes(),
        )
        .await?;
    Ok((segment, network))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proofs_bind_role_segment_subnet_and_fresh_nonces() {
        let subnet = [10, 214, 0, 0, 24];
        let other_subnet = [10, 214, 1, 0, 24];
        let original = proof("secret", b"client", b"wan", &subnet, &[1; 32], &[2; 32])
            .finalize()
            .into_bytes();
        assert!(
            proof("secret", b"client", b"wan", &subnet, &[1; 32], &[2; 32])
                .verify_slice(&original)
                .is_ok()
        );
        for candidate in [
            proof("wrong", b"client", b"wan", &subnet, &[1; 32], &[2; 32]),
            proof("secret", b"server", b"wan", &subnet, &[1; 32], &[2; 32]),
            proof("secret", b"client", b"other", &subnet, &[1; 32], &[2; 32]),
            proof("secret", b"client", b"wan", &other_subnet, &[1; 32], &[2; 32]),
            proof("secret", b"client", b"wan", &subnet, &[3; 32], &[2; 32]),
            proof("secret", b"client", b"wan", &subnet, &[1; 32], &[3; 32]),
        ] {
            assert!(candidate.verify_slice(&original).is_err());
        }
    }

    #[tokio::test]
    async fn silent_peer_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                server(&mut stream, "secret")
            )
            .await
            .is_err()
        );
    }
}

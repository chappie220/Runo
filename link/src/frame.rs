//! Frames: a 4-byte big-endian payload length, a 4-byte channel, then the
//! payload. Channel 0 carries JSON (see `proto`); the others carry the
//! bytes of a terminal or a file, and an empty payload ends a file.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The control channel.
pub const CONTROL: u32 = 0;
/// The largest payload; data is sent in pieces this size or smaller.
pub const MAX_PAYLOAD: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub channel: u32,
    pub data: Vec<u8>,
}

/// The next frame, `None` at a clean end of the stream.
pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Frame>> {
    let mut header = [0u8; 8];
    match reader.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    reader.read_exact(&mut header[1..]).await?;
    let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    let channel = u32::from_be_bytes(header[4..].try_into().unwrap());
    if length > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a frame of {length} bytes is too large"),
        ));
    }
    let mut data = vec![0u8; length];
    reader.read_exact(&mut data).await?;
    Ok(Some(Frame { channel, data }))
}

pub async fn write<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> std::io::Result<()> {
    if frame.data.len() > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut bytes = Vec::with_capacity(8 + frame.data.len());
    bytes.extend_from_slice(&(frame.data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&frame.channel.to_be_bytes());
    bytes.extend_from_slice(&frame.data);
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_oversized_ones_are_refused() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let frames = [
            Frame {
                channel: 0,
                data: b"{}".to_vec(),
            },
            Frame {
                channel: 7,
                data: vec![],
            },
        ];
        for frame in &frames {
            write(&mut a, frame).await.unwrap();
        }
        drop(a);
        assert_eq!(read(&mut b).await.unwrap().as_ref(), Some(&frames[0]));
        assert_eq!(read(&mut b).await.unwrap().as_ref(), Some(&frames[1]));
        assert_eq!(read(&mut b).await.unwrap(), None);

        let (mut a, mut b) = tokio::io::duplex(64);
        let mut header = ((MAX_PAYLOAD + 1) as u32).to_be_bytes().to_vec();
        header.extend_from_slice(&1u32.to_be_bytes());
        a.write_all(&header).await.unwrap();
        assert!(read(&mut b).await.is_err());
        let big = Frame {
            channel: 1,
            data: vec![0; MAX_PAYLOAD + 1],
        };
        assert!(write(&mut a, &big).await.is_err());
    }
}

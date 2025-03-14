mod ext {
    use std::io::Result;
    use tokio::io::AsyncReadExt;
    use bytes::Buf;

    pub trait StreamExt: AsyncReadExt + Unpin {
        async fn read_u8(&mut self) -> Result<u8> {
            let mut buf = [0u8; 1];
            self.read_exact(&mut buf).await?;
            Ok(buf[0])
        }

        async fn read_u16(&mut self) -> Result<u16> {
            let mut buf = [0u8; 2];
            self.read_exact(&mut buf).await?;
            Ok(u16::from_be_bytes(buf))
        }

        async fn read_u32(&mut self) -> Result<u32> {
            let mut buf = [0u8; 4];
            self.read_exact(&mut buf).await?;
            Ok(u32::from_be_bytes(buf))
        }

        async fn read_u128(&mut self) -> Result<u128> {
            let mut buf = [0u8; 16];
            self.read_exact(&mut buf).await?;
            Ok(u128::from_be_bytes(buf))
        }

        async fn read_string(&mut self, n: usize) -> Result<String> {
            let bytes = self.read_bytes(n).await?;
            String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }

        async fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
            let mut buffer = vec![0u8; n];
            self.read_exact(&mut buffer).await?;
            Ok(buffer)
        }
    }
}

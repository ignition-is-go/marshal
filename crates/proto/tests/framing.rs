use proto::framing::{read_frame, write_frame, FrameError, MAX_FRAME};
use tokio::io::duplex;

#[tokio::test]
async fn write_then_read_roundtrips() {
    let (mut a, mut b) = duplex(64 * 1024);
    write_frame(&mut a, b"hello").await.unwrap();
    let got = read_frame(&mut b).await.unwrap();
    assert_eq!(got, b"hello");
}

#[tokio::test]
async fn read_handles_partial_writes() {
    use tokio::io::AsyncWriteExt;
    let (mut a, mut b) = duplex(64 * 1024);

    let writer = tokio::spawn(async move {
        // 4-byte LE length = 5
        a.write_all(&5u32.to_le_bytes()[..2]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        a.write_all(&5u32.to_le_bytes()[2..]).await.unwrap();
        a.write_all(b"he").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        a.write_all(b"llo").await.unwrap();
    });

    let got = read_frame(&mut b).await.unwrap();
    assert_eq!(got, b"hello");
    writer.await.unwrap();
}

#[tokio::test]
async fn read_rejects_oversized_frame() {
    use tokio::io::AsyncWriteExt;
    let (mut a, mut b) = duplex(64 * 1024);
    let too_big = (MAX_FRAME as u32 + 1).to_le_bytes();
    a.write_all(&too_big).await.unwrap();
    let err = read_frame(&mut b).await.unwrap_err();
    assert!(matches!(err, FrameError::TooLarge(_)));
}

#[tokio::test]
async fn write_rejects_oversized_payload() {
    let (mut a, _b) = duplex(64);
    let big = vec![0u8; MAX_FRAME + 1];
    let err = write_frame(&mut a, &big).await.unwrap_err();
    assert!(matches!(err, FrameError::TooLarge(_)));
}

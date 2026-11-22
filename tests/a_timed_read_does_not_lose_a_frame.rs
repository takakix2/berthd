//! 時間切れつきの読みが、フレームの途中で時間切れになっても、そのフレームを失わないこと。
//!
//! 検体はどれも「しばらく待って、来なければ次へ進む」ためにフレームを時間切れつきで読む。
//! その読みが半分読んだフレームを捨てると、以後その接続はフレームの境目がずれ、落ちる検体が
//! 回ごとに入れ替わる（出力が来ない・制御フレームが来ない・データが制御として読まれる）。
//! ここでは書く側を意図的にフレームの途中で止めて、その分かれ目に時間切れを落とす。

use berthd::{control_frame, decode_control, write_frame, Control, Frame, KIND_CONTROL, KIND_DATA};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

mod common;
use common::read_frame_within;

/// 1 フレームのバイト列（`write_frame` が線に書く物）。
async fn bytes_of(f: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    write_frame(&mut out, f).await.unwrap();
    out
}

#[tokio::test(flavor = "current_thread")]
async fn a_frame_that_arrives_in_two_pieces_survives_a_timeout_between_them() {
    let (mut tx, mut rx) = tokio::io::duplex(4096);

    let data = Frame::data(7, b"\x1b]9;9;\"C:\\Users\\someone\"\x1b\\".to_vec());
    let wire = bytes_of(&data).await;
    let ctrl = control_frame(berthd::SOURCE, 8, &Control::List);
    let ctrl_wire = bytes_of(&ctrl).await;

    // ① まだ何も来ていない: 時間切れは `None`。
    assert!(
        read_frame_within(&mut rx, Duration::from_millis(50))
            .await
            .is_none(),
        "何も書いていないのにフレームが返った"
    );

    // ② 長さの 4 バイトだけ書いて止める。残りは、読む側の締切を過ぎてから書く。
    tx.write_all(&wire[..4]).await.unwrap();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.write_all(&wire[4..]).await.unwrap();
        tx.write_all(&ctrl_wire).await.unwrap();
        tx
    });

    // 読み始めたフレームは、締切（50ms）を過ぎても最後まで読んで返す。
    let got = read_frame_within(&mut rx, Duration::from_millis(50))
        .await
        .expect("届き始めたフレームを時間切れで捨てた")
        .unwrap();
    assert_eq!(got.kind, KIND_DATA);
    assert_eq!(got.serial, 7);
    assert_eq!(got.payload, data.payload);

    // ③ 次のフレームも境目がずれずに読める（ずれていれば種類も中身も化ける）。
    let next = read_frame_within(&mut rx, Duration::from_secs(5))
        .await
        .expect("次のフレームが来ない")
        .unwrap();
    assert_eq!(next.kind, KIND_CONTROL);
    assert_eq!(next.serial, 8);
    assert!(
        matches!(decode_control(&next.payload), Ok(Control::List)),
        "次のフレームが制御として読めない: {:?}",
        String::from_utf8_lossy(&next.payload)
    );

    // ④ 書く側が閉じたら、待たずに EOF のエラーを返す（時間切れと区別できる）。
    drop(writer.await.unwrap());
    let end = read_frame_within(&mut rx, Duration::from_secs(5)).await;
    assert!(
        matches!(end, Some(Err(_))),
        "閉じた接続が EOF として返らない"
    );
}

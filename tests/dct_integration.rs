//! baseline JPEG（DCTDecode）デコーダの統合テスト。
//!
//! フィクスチャ `tests/fixtures/*.jpg` と期待値 `*.rgb` は
//! `.NET System.Drawing`（外部ツール）で生成済み（生成手順は
//! `tests/fixtures/gen_jpeg_fixtures.ps1` を参照）。期待値は同じ JPEG を
//! .NET 自身でデコードした RGB なので、IDCT 実装差で完全一致はしない。
//! そのため誤差を許容して比較する:
//!   - 各ピクセル各チャネルの絶対差 <= 10
//!   - 全体の平均絶対差 <= 2.0
//!
//! System.Drawing は CMYK / グレースケール JPEG を素直に生成できないため、
//! ここではカラー（YCbCr→RGB）のみを実画像で検証する。CMYK / グレースケールの
//! 経路はライブラリ側の単体テスト（`src/filters/dct.rs`）でカバーする。

use pdf_rust::filters::dct;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push(name);
    p
}

/// JPEG をデコードして RGB 期待値と誤差許容で比較する。
fn check_jpeg(jpg: &str, rgb: &str) {
    let jpg_path = fixture(jpg);
    let rgb_path = fixture(rgb);
    if !jpg_path.exists() || !rgb_path.exists() {
        eprintln!(
            "フィクスチャ {jpg} / {rgb} が無いためスキップ（gen_jpeg_fixtures.ps1 を実行のこと）"
        );
        return;
    }
    let jpg_data = std::fs::read(&jpg_path).unwrap();
    let expected = std::fs::read(&rgb_path).unwrap();

    let img = dct::decode(&jpg_data).unwrap_or_else(|e| panic!("{jpg} のデコードに失敗: {e}"));
    assert_eq!(img.components, 3, "{jpg}: RGB 3 成分のはず");
    assert_eq!(
        img.data.len(),
        expected.len(),
        "{jpg}: ピクセルバイト数が期待値と異なる（{}x{}）",
        img.width,
        img.height
    );

    let mut max_diff = 0i32;
    let mut sum_diff = 0u64;
    for (i, (&got, &exp)) in img.data.iter().zip(expected.iter()).enumerate() {
        let d = (got as i32 - exp as i32).abs();
        if d > max_diff {
            max_diff = d;
        }
        sum_diff += d as u64;
        assert!(
            d <= 10,
            "{jpg}: バイト {i} の差が大きすぎる (got={got}, exp={exp}, diff={d})"
        );
    }
    let avg = sum_diff as f64 / img.data.len() as f64;
    assert!(
        avg <= 2.0,
        "{jpg}: 平均絶対差が大きすぎる (avg={avg:.3}, max={max_diff})"
    );
    eprintln!(
        "{jpg}: OK ({}x{}, max_diff={max_diff}, avg={avg:.3})",
        img.width, img.height
    );
}

#[test]
fn solid_16_q90() {
    check_jpeg("solid_16_q90.jpg", "solid_16_q90.rgb");
}

#[test]
fn gradient_16_q90() {
    check_jpeg("gradient_16_q90.jpg", "gradient_16_q90.rgb");
}

#[test]
fn blocks_16_q90() {
    check_jpeg("blocks_16_q90.jpg", "blocks_16_q90.rgb");
}

#[test]
fn blocks_16_q50() {
    // 低品質 = 4:2:0 サブサンプリング。クロマアップサンプリングの検証。
    check_jpeg("blocks_16_q50.jpg", "blocks_16_q50.rgb");
}

#[test]
fn gradient_17x13_q90() {
    // 奇数サイズ = 右端・下端の半端 MCU を検証。
    check_jpeg("gradient_17x13_q90.jpg", "gradient_17x13_q90.rgb");
}

#[test]
fn blocks_17x13_q50() {
    // 奇数サイズ + 4:2:0。半端 MCU とサブサンプリングの組み合わせ。
    check_jpeg("blocks_17x13_q50.jpg", "blocks_17x13_q50.rgb");
}

#[test]
fn gradient_32_q50() {
    // 複数 MCU にまたがるグラデーション（4:2:0）。
    check_jpeg("gradient_32_q50.jpg", "gradient_32_q50.rgb");
}

#[test]
fn sof2_progressive_is_rejected() {
    // SOF2（progressive）は明示的な Filter エラーで拒否されること。
    // 最小の SOI + SOF2 マーカーを合成して投げる。
    let mut data = vec![0xFF, 0xD8]; // SOI
                                     // SOF2: marker, len(=11), prec=8, h=8, w=8, nc=1, comp(id=1, hv=0x11, tq=0)
    data.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01]);
    data.extend_from_slice(&[0x01, 0x11, 0x00]);
    data.extend_from_slice(&[0xFF, 0xD9]); // EOI
    match dct::decode(&data) {
        Ok(_) => panic!("progressive は拒否されるべき"),
        Err(e) => {
            let msg = format!("{e}");
            assert!(msg.contains("progressive"), "エラーメッセージ: {msg}");
        }
    }
}

#[test]
fn truncated_inputs_do_not_panic() {
    // 各フィクスチャを先頭から数バイト刻みで切り詰めて投げ、panic しないことを確認。
    for name in [
        "blocks_16_q50.jpg",
        "gradient_17x13_q90.jpg",
        "gradient_32_q50.jpg",
    ] {
        let path = fixture(name);
        if !path.exists() {
            eprintln!("{name} が無いためスキップ");
            continue;
        }
        let data = std::fs::read(&path).unwrap();
        let mut len = 0;
        while len <= data.len() {
            // Err でも Ok（縮退）でもよい。panic しないことが要件。
            let _ = dct::decode(&data[..len]);
            len += 7; // 数バイト刻み
        }
        // 末尾 1 バイト欠けも確認
        if !data.is_empty() {
            let _ = dct::decode(&data[..data.len() - 1]);
        }
    }
}

#[test]
fn random_corruption_does_not_panic() {
    // 固定シードの簡易 LCG でランダムにバイトを破壊し、panic しないことを確認。
    let path = fixture("blocks_16_q50.jpg");
    if !path.exists() {
        eprintln!("blocks_16_q50.jpg が無いためスキップ");
        return;
    }
    let base = std::fs::read(&path).unwrap();
    let mut seed: u64 = 0x1234_5678_9abc_def0;
    let mut next = || {
        // 線形合同法（Numerical Recipes 系の定数）
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as u32
    };
    for _ in 0..200 {
        let mut d = base.clone();
        // 1〜8 箇所をランダムに破壊
        let n = 1 + (next() % 8) as usize;
        for _ in 0..n {
            if d.is_empty() {
                break;
            }
            let idx = (next() as usize) % d.len();
            d[idx] = (next() & 0xFF) as u8;
        }
        let _ = dct::decode(&d); // panic しなければよい
    }

    // 完全ランダムなバイト列も投げる
    for _ in 0..50 {
        let len = (next() % 512) as usize;
        let mut d = vec![0u8; len];
        for b in d.iter_mut() {
            *b = (next() & 0xFF) as u8;
        }
        let _ = dct::decode(&d);
    }
}

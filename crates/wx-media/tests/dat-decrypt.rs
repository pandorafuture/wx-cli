use wx_media::{DatDecryptOptions, DatFormat, MediaError};

// ── Helpers ──────────────────────────────────────────────────────────

/// Build a simple XOR-encrypted `.dat` from a known JPEG header.
fn make_xor_dat(key: u8) -> Vec<u8> {
    // Minimal JPEG: FF D8 FF E0 + padding
    let plain = [
        0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00,
    ];
    plain.iter().map(|b| b ^ key).collect()
}

/// Build a V1-encrypted `.dat` file.
/// Header: 07 08 V1 08 07 (6B) + aes_size LE (4B) + xor_size LE (4B) + 0x01 (1B) = 15B
/// Then AES-ECB encrypted payload with fixed key, then raw, then XOR tail.
fn make_v1_dat() -> Vec<u8> {
    use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
    use aes::Aes128;

    let key = b"cfcd208495d565ef"; // md5("0")[:16]
    let cipher = Aes128::new(key.into());

    // Plaintext: JPEG header (16 bytes = 1 AES block) with PKCS7 padding
    let mut block1 = Array::from([
        0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01,
    ]);
    let mut block2 = Array::from([16u8; 16]); // full PKCS7 padding block

    cipher.encrypt_block(&mut block1);
    cipher.encrypt_block(&mut block2);

    let aes_size: u32 = 16; // original plaintext size
    let xor_size: u32 = 0;

    let mut dat = Vec::new();
    dat.extend_from_slice(b"\x07\x08V1\x08\x07"); // 6B signature
    dat.extend_from_slice(&aes_size.to_le_bytes()); // 4B aes_size
    dat.extend_from_slice(&xor_size.to_le_bytes()); // 4B xor_size
    dat.push(0x01); // 1B padding
    dat.extend_from_slice(&block1); // AES ciphertext block 1
    dat.extend_from_slice(&block2); // AES ciphertext block 2 (PKCS7 padding)
    dat
}

/// Build a V2-encrypted `.dat` file with known AES key and XOR tail.
fn make_v2_dat(aes_key: &[u8; 16], xor_key: u8) -> Vec<u8> {
    use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
    use aes::Aes128;

    let cipher = Aes128::new(aes_key.into());

    // Plaintext: PNG header (16 bytes = 1 block)
    let mut block1 = Array::from([
        0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52,
    ]);
    let mut block2 = Array::from([16u8; 16]); // PKCS7 padding block

    cipher.encrypt_block(&mut block1);
    cipher.encrypt_block(&mut block2);

    let aes_size: u32 = 16;
    // Tail: 4 bytes XOR-encrypted
    let xor_plain = [0x00u8, 0x00, 0x00, 0x00];
    let xor_enc: Vec<u8> = xor_plain.iter().map(|b| b ^ xor_key).collect();
    let xor_size: u32 = xor_enc.len() as u32;

    // Middle raw section: 8 bytes of unencrypted data
    let raw_data = [0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44];

    let mut dat = Vec::new();
    dat.extend_from_slice(b"\x07\x08V2\x08\x07"); // 6B signature
    dat.extend_from_slice(&aes_size.to_le_bytes()); // 4B aes_size
    dat.extend_from_slice(&xor_size.to_le_bytes()); // 4B xor_size
    dat.push(0x01); // 1B padding
    dat.extend_from_slice(&block1); // AES ciphertext
    dat.extend_from_slice(&block2); // PKCS7 padding ciphertext
    dat.extend_from_slice(&raw_data); // unencrypted middle
    dat.extend_from_slice(&xor_enc); // XOR tail
    dat
}

// ── XOR tests ────────────────────────────────────────────────────────

#[test]
fn xor_decrypt_jpg() {
    let dat = make_xor_dat(0xAB);
    let result = wx_media::decrypt_dat(&dat, &DatDecryptOptions::default()).unwrap();
    assert_eq!(result.format, DatFormat::Xor);
    assert_eq!(result.ext, "jpg");
    assert_eq!(&result.data[..3], &[0xFF, 0xD8, 0xFF]);
}

#[test]
fn xor_decrypt_png() {
    let plain = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let key = 0x55u8;
    let dat: Vec<u8> = plain.iter().map(|b| b ^ key).collect();
    let result = wx_media::decrypt_dat(&dat, &DatDecryptOptions::default()).unwrap();
    assert_eq!(result.format, DatFormat::Xor);
    assert_eq!(result.ext, "png");
}

#[test]
fn xor_detect_gif() {
    let plain = [0x47u8, 0x49, 0x46, 0x38, 0x39, 0x61]; // GIF89a
    let key = 0x12u8;
    let dat: Vec<u8> = plain.iter().map(|b| b ^ key).collect();
    let result = wx_media::decrypt_dat(&dat, &DatDecryptOptions::default()).unwrap();
    assert_eq!(result.ext, "gif");
}

#[test]
fn xor_detect_wxgf() {
    let plain = [0x77u8, 0x78, 0x67, 0x66, 0x00, 0x01];
    let key = 0x24u8;
    let dat: Vec<u8> = plain.iter().map(|b| b ^ key).collect();
    let result = wx_media::decrypt_dat(&dat, &DatDecryptOptions::default()).unwrap();
    assert_eq!(result.ext, "wxgf");
    assert_eq!(&result.data[..4], b"wxgf");
}

#[test]
fn xor_header_too_short() {
    let dat = vec![0xAB, 0xCD]; // Only 2 bytes, not enough for reliable detection
    let result = wx_media::decrypt_dat(&dat, &DatDecryptOptions::default());
    // Should fail since no 3+ byte magic matches
    assert!(result.is_err());
}

// ── V1 tests ─────────────────────────────────────────────────────────

#[test]
fn v1_decrypt_success() {
    let dat = make_v1_dat();
    let result = wx_media::decrypt_dat(&dat, &DatDecryptOptions::default()).unwrap();
    assert_eq!(result.format, DatFormat::V1);
    assert_eq!(result.ext, "jpg");
    assert_eq!(&result.data[..3], &[0xFF, 0xD8, 0xFF]);
}

// ── V2 tests ─────────────────────────────────────────────────────────

#[test]
fn v2_decrypt_success() {
    let aes_key = b"abcdefghijklmnop";
    let xor_key = 0x37u8;
    let dat = make_v2_dat(aes_key, xor_key);

    let opts = DatDecryptOptions {
        v2_aes_key: Some(*aes_key),
        xor_key: Some(xor_key),
    };
    let result = wx_media::decrypt_dat(&dat, &opts).unwrap();
    assert_eq!(result.format, DatFormat::V2);
    assert_eq!(result.ext, "png");
    assert_eq!(&result.data[..4], &[0x89, 0x50, 0x4E, 0x47]);
    // Verify raw middle section preserved
    assert_eq!(
        &result.data[16..24],
        &[0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44]
    );
    // Verify XOR tail decrypted
    assert_eq!(&result.data[24..28], &[0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn v2_missing_key() {
    let aes_key = b"abcdefghijklmnop";
    let dat = make_v2_dat(aes_key, 0x37);
    let result = wx_media::decrypt_dat(&dat, &DatDecryptOptions::default());
    assert!(matches!(result, Err(MediaError::MissingV2Key)));
}

#[test]
fn v2_wrong_key() {
    let aes_key = b"abcdefghijklmnop";
    let dat = make_v2_dat(aes_key, 0x37);
    let wrong_key = b"0000000000000000";
    let opts = DatDecryptOptions {
        v2_aes_key: Some(*wrong_key),
        xor_key: Some(0x37),
    };
    // Wrong key → PKCS7 validation fails → AesDecryptFailed error
    let result = wx_media::decrypt_dat(&dat, &opts);
    assert!(matches!(result, Err(MediaError::AesDecryptFailed { .. })));
}

// ── Edge cases ───────────────────────────────────────────────────────

#[test]
fn empty_input() {
    let result = wx_media::decrypt_dat(&[], &DatDecryptOptions::default());
    assert!(result.is_err());
}

#[test]
fn truncated_v2_header() {
    // V2 signature but truncated before aes_size field
    let dat = b"\x07\x08V2\x08\x07\x00\x04";
    let opts = DatDecryptOptions {
        v2_aes_key: Some(*b"abcdefghijklmnop"),
        xor_key: Some(0x37),
    };
    let result = wx_media::decrypt_dat(dat, &opts);
    assert!(result.is_err());
}

// ── detect_dat_format tests ──────────────────────────────────────────

#[test]
fn detect_format_xor() {
    let dat = make_xor_dat(0xAB);
    assert_eq!(wx_media::detect_dat_format(&dat), None);
    // XOR format is detected by elimination (no V1/V2 signature), returns None for the enum
}

#[test]
fn detect_format_v1() {
    let dat = make_v1_dat();
    assert_eq!(wx_media::detect_dat_format(&dat), Some(DatFormat::V1));
}

#[test]
fn detect_format_v2() {
    let aes_key = b"abcdefghijklmnop";
    let dat = make_v2_dat(aes_key, 0x37);
    assert_eq!(wx_media::detect_dat_format(&dat), Some(DatFormat::V2));
}

// ── detect_image_type tests ──────────────────────────────────────────

#[test]
fn detect_image_types() {
    use wx_media::ImageType;

    assert_eq!(
        wx_media::detect_image_type(&[0xFF, 0xD8, 0xFF]),
        ImageType::Jpg
    );
    assert_eq!(
        wx_media::detect_image_type(&[0x89, 0x50, 0x4E, 0x47]),
        ImageType::Png
    );
    assert_eq!(
        wx_media::detect_image_type(&[0x47, 0x49, 0x46, 0x38]),
        ImageType::Gif
    );
    assert_eq!(
        wx_media::detect_image_type(&[
            0x52, 0x49, 0x46, 0x46, 0x00, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50
        ]),
        ImageType::Webp
    );
    assert_eq!(
        wx_media::detect_image_type(&[0x49, 0x49, 0x2A, 0x00]),
        ImageType::Tif
    );
    assert_eq!(
        wx_media::detect_image_type(&[0x77, 0x78, 0x67, 0x66]),
        ImageType::Wxgf
    );
    assert_eq!(
        wx_media::detect_image_type(&[0x00, 0x00, 0x00, 0x00]),
        ImageType::Unknown
    );
}

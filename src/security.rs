//! 標準セキュリティハンドラ（PDF 32000-1:2008 §7.6.3 / 2.0 §7.6.4）。
//!
//! 対応:
//! - V1 / R2: 40bit RC4
//! - V2 / R3: 40-128bit RC4
//! - V4 / R4: 128bit RC4 または AES-128（`/CF` `/StmF` `/StrF`）
//! - V5 / R6: 256bit AES（PDF 2.0）
//!
//! V5 R5 は ISO で取り消された暫定方式のため未対応（古い Acrobat の限られた
//! バージョンしか生成しない）。空ユーザーパスワード PDF を読めるようにする
//! のが第一目的。

use crate::crypto::{aes, md5, rc4, sha2};
use crate::error::{PdfError, Result};
use crate::object::{Dictionary, Object, ObjectId};

/// 標準セキュリティハンドラがオブジェクトの暗号化に使う 1 つのアルゴリズム。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CryptMethod {
    /// 暗号化しない（`/Identity` または V4 CF=None）。
    Identity,
    /// RC4（V1/V2/V4 の `V2` CFM）。鍵長は [`StandardHandler::key_len`]。
    Rc4,
    /// AES-128 CBC（V4 の `AESV2` CFM）。
    AesV2,
    /// AES-256 CBC（V5 の `AESV3` CFM）。
    AesV3,
}

/// パース済みの `/Encrypt` 辞書とそれに紐づく派生ファイル鍵。
#[derive(Debug)]
pub struct StandardHandler {
    /// PDF 2.0 R6 の場合は 32 バイト、それ以外は 5..=16 バイト。
    file_key: Vec<u8>,
    /// V（暗号スキームのバージョン）。1/2/4/5。
    v: i64,
    /// R（標準ハンドラの改訂番号）。2/3/4/5/6。
    r: i64,
    /// 文字列に適用する暗号化。
    str_method: CryptMethod,
    /// ストリームに適用する暗号化。
    stm_method: CryptMethod,
}

/// PDF 標準のパスワードパディング（32 バイト、Algorithm 2 step a）。
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

impl StandardHandler {
    /// `/Encrypt` 辞書とトレーラの `/ID` から、`password` で開けるか試す。
    /// 開ければ復号鍵を保持したハンドラを返す。`/ID` は最低 1 要素必要
    /// （R2-R4 のときに使うため。R6 では参照しない）。
    ///
    /// `resolve` は辞書内の間接参照を平坦化するためのコールバック。
    pub fn new(
        encrypt: &Dictionary,
        ids: &[Vec<u8>],
        password: &[u8],
        resolve: impl Fn(&Object) -> Object,
    ) -> Result<StandardHandler> {
        // /Filter は /Standard でなければならない。
        match encrypt.get("Filter").map(&resolve) {
            Some(Object::Name(n)) if n == "Standard" => {}
            _ => return Err(PdfError::Invalid("non-standard /Filter in /Encrypt".into())),
        }

        let v = get_int(encrypt, "V", &resolve).unwrap_or(0);
        let r = get_int(encrypt, "R", &resolve)
            .ok_or_else(|| PdfError::Invalid("missing /R in /Encrypt".into()))?;
        // /Length はビット単位。V1 は 40、V2/V4 は 40..=128、V5 は 256。
        let length_bits = get_int(encrypt, "Length", &resolve).unwrap_or(40);
        let p = get_int(encrypt, "P", &resolve)
            .ok_or_else(|| PdfError::Invalid("missing /P in /Encrypt".into()))?;
        let o = get_string(encrypt, "O", &resolve)
            .ok_or_else(|| PdfError::Invalid("missing /O in /Encrypt".into()))?;
        let u = get_string(encrypt, "U", &resolve)
            .ok_or_else(|| PdfError::Invalid("missing /U in /Encrypt".into()))?;
        let encrypt_metadata = match encrypt.get("EncryptMetadata").map(&resolve) {
            Some(Object::Boolean(b)) => b,
            _ => true,
        };

        // /CF /StmF /StrF（V4/V5）を解釈してストリーム/文字列の方式を決める。
        let (str_method, stm_method) = if v == 4 || v == 5 {
            let stmf = match encrypt.get("StmF").map(&resolve) {
                Some(Object::Name(n)) => n,
                _ => "Identity".into(),
            };
            let strf = match encrypt.get("StrF").map(&resolve) {
                Some(Object::Name(n)) => n,
                _ => "Identity".into(),
            };
            let cf = match encrypt.get("CF").map(&resolve) {
                Some(Object::Dictionary(d)) => d,
                _ => Dictionary::new(),
            };
            let lookup = |name: &str| -> CryptMethod {
                if name == "Identity" {
                    return CryptMethod::Identity;
                }
                let sub = match cf.get(name).map(&resolve) {
                    Some(Object::Dictionary(d)) => d,
                    _ => return CryptMethod::Identity,
                };
                match sub.get("CFM").map(&resolve) {
                    Some(Object::Name(n)) => match n.as_str() {
                        "V2" => CryptMethod::Rc4,
                        "AESV2" => CryptMethod::AesV2,
                        "AESV3" => CryptMethod::AesV3,
                        _ => CryptMethod::Identity,
                    },
                    _ => CryptMethod::Identity,
                }
            };
            (lookup(&strf), lookup(&stmf))
        } else {
            // V1/V2 は RC4 のみ。
            (CryptMethod::Rc4, CryptMethod::Rc4)
        };

        let id0 = ids.first().cloned().unwrap_or_default();

        // 鍵生成と認証はバージョンで分岐。
        let file_key = if r >= 6 {
            // PDF 2.0: U/UE と O/OE を使う AES-256 ベース。
            let ue = get_string(encrypt, "UE", &resolve)
                .ok_or_else(|| PdfError::Invalid("missing /UE in V5/R6 /Encrypt".into()))?;
            let oe = get_string(encrypt, "OE", &resolve)
                .ok_or_else(|| PdfError::Invalid("missing /OE in V5/R6 /Encrypt".into()))?;
            authenticate_r6(password, &u, &ue, &o, &oe)?
        } else {
            // V1/V2/V4
            authenticate_r2_to_r4(
                password,
                r,
                length_bits as usize,
                p,
                &o,
                &u,
                &id0,
                encrypt_metadata,
            )?
        };

        Ok(StandardHandler {
            file_key,
            v,
            r,
            str_method,
            stm_method,
        })
    }

    /// 文字列を復号する（`obj_id` はその文字列を含む間接オブジェクトの ID）。
    pub fn decrypt_string(&self, obj_id: ObjectId, data: &[u8]) -> Result<Vec<u8>> {
        self.decrypt(self.str_method, obj_id, data)
    }

    /// ストリームを復号する。`/Filter` 適用の前段で呼ぶ。
    pub fn decrypt_stream(&self, obj_id: ObjectId, data: &[u8]) -> Result<Vec<u8>> {
        self.decrypt(self.stm_method, obj_id, data)
    }

    fn decrypt(&self, method: CryptMethod, obj_id: ObjectId, data: &[u8]) -> Result<Vec<u8>> {
        match method {
            CryptMethod::Identity => Ok(data.to_vec()),
            CryptMethod::Rc4 => {
                let key = self.object_key(obj_id, false);
                Ok(rc4::rc4(&key, data))
            }
            CryptMethod::AesV2 => {
                let key = self.object_key(obj_id, true);
                aes::aes_cbc_decrypt_pkcs5(&key, data)
            }
            CryptMethod::AesV3 => aes::aes_cbc_decrypt_pkcs5(&self.file_key, data),
        }
    }

    /// V1/V2/V4 のオブジェクト鍵生成（PDF 32000 §7.6.2 Algorithm 1）。
    fn object_key(&self, obj_id: ObjectId, is_aes: bool) -> Vec<u8> {
        let (num, gen) = obj_id;
        let mut buf = Vec::with_capacity(self.file_key.len() + 9);
        buf.extend_from_slice(&self.file_key);
        buf.push((num & 0xff) as u8);
        buf.push(((num >> 8) & 0xff) as u8);
        buf.push(((num >> 16) & 0xff) as u8);
        buf.push((gen & 0xff) as u8);
        buf.push(((gen >> 8) & 0xff) as u8);
        if is_aes {
            buf.extend_from_slice(b"sAlT");
        }
        let h = md5::md5(&buf);
        let take = (self.file_key.len() + 5).min(16);
        h[..take].to_vec()
    }

    /// 現在のセキュリティハンドラのバージョンと改訂番号（デバッグ用）。
    pub fn version(&self) -> (i64, i64) {
        (self.v, self.r)
    }
}

// ---------------------------------------------------------------------------
// V1/V2/V4 (R2/R3/R4) の鍵生成と認証
// ---------------------------------------------------------------------------

/// PDF 32000 §7.6.3.3 Algorithm 2: ユーザーパスワードからファイル鍵を計算。
#[allow(clippy::too_many_arguments)]
fn compute_file_key_r2_r4(
    password: &[u8],
    r: i64,
    length_bits: usize,
    p: i64,
    o: &[u8],
    id0: &[u8],
    encrypt_metadata: bool,
) -> Vec<u8> {
    let key_len = (length_bits / 8).clamp(5, 16);
    let padded = pad_password(password);

    let mut state = Vec::with_capacity(32 + o.len() + 4 + id0.len() + 4);
    state.extend_from_slice(&padded);
    state.extend_from_slice(o);
    // P は符号付き 32bit。リトルエンディアン 4 バイト。
    let p_le = (p as i32 as u32).to_le_bytes();
    state.extend_from_slice(&p_le);
    state.extend_from_slice(id0);
    if r >= 4 && !encrypt_metadata {
        state.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    }

    let mut digest = md5::md5(&state).to_vec();
    if r >= 3 {
        for _ in 0..50 {
            let h = md5::md5(&digest[..key_len]);
            digest[..16].copy_from_slice(&h);
        }
    }
    digest.truncate(key_len);
    digest
}

/// パスワードを 32 バイトに切り詰め/パディングする（Algorithm 2 step a）。
fn pad_password(password: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = password.len().min(32);
    out[..n].copy_from_slice(&password[..n]);
    if n < 32 {
        out[n..].copy_from_slice(&PAD[..32 - n]);
    }
    out
}

/// PDF 32000 §7.6.3.4 Algorithm 5: ファイル鍵から U エントリを計算する。
/// /U と比較して認証に使う。
fn compute_u_entry_r2_r4(file_key: &[u8], r: i64, id0: &[u8]) -> Vec<u8> {
    if r == 2 {
        // U = RC4(file_key, PAD)
        rc4::rc4(file_key, &PAD)
    } else {
        // R >= 3
        // a) MD5(PAD + id0)
        let mut buf = Vec::with_capacity(32 + id0.len());
        buf.extend_from_slice(&PAD);
        buf.extend_from_slice(id0);
        let h = md5::md5(&buf);
        // b) RC4(file_key, h)
        let mut e = rc4::rc4(file_key, &h).to_vec();
        // c) for i in 1..=19: RC4(file_key XOR i, e)
        for i in 1u8..=19 {
            let xkey: Vec<u8> = file_key.iter().map(|b| b ^ i).collect();
            e = rc4::rc4(&xkey, &e);
        }
        // d) Pad to 32 bytes with arbitrary（ここでは 0 で埋める）。
        e.resize(32, 0);
        e
    }
}

/// ユーザーパスワードか所有者パスワードで認証し、ファイル鍵を返す。
#[allow(clippy::too_many_arguments)]
fn authenticate_r2_to_r4(
    password: &[u8],
    r: i64,
    length_bits: usize,
    p: i64,
    o: &[u8],
    u: &[u8],
    id0: &[u8],
    encrypt_metadata: bool,
) -> Result<Vec<u8>> {
    // 1) まずユーザーパスワードとして試す。
    let key = compute_file_key_r2_r4(password, r, length_bits, p, o, id0, encrypt_metadata);
    let computed_u = compute_u_entry_r2_r4(&key, r, id0);
    if check_u(&computed_u, u, r) {
        return Ok(key);
    }
    // 2) 所有者パスワードとして試す（Algorithm 7）。
    let owner_pass = recover_user_password_r2_r4(password, r, o)?;
    let key = compute_file_key_r2_r4(&owner_pass, r, length_bits, p, o, id0, encrypt_metadata);
    let computed_u = compute_u_entry_r2_r4(&key, r, id0);
    if check_u(&computed_u, u, r) {
        return Ok(key);
    }
    Err(PdfError::Invalid(
        "PDF requires a non-empty password to decrypt".into(),
    ))
}

/// /U エントリの比較。R==2 では 32 バイト、R>=3 では先頭 16 バイトのみ比較。
fn check_u(computed: &[u8], stored: &[u8], r: i64) -> bool {
    if r == 2 {
        computed.len() >= 32 && stored.len() >= 32 && computed[..32] == stored[..32]
    } else {
        computed.len() >= 16 && stored.len() >= 16 && computed[..16] == stored[..16]
    }
}

/// 所有者パスワードからユーザーパスワード相当を復元（Algorithm 7）。
fn recover_user_password_r2_r4(password: &[u8], r: i64, o: &[u8]) -> Result<Vec<u8>> {
    let padded = pad_password(password);
    let mut digest = md5::md5(&padded);
    if r >= 3 {
        for _ in 0..50 {
            digest = md5::md5(&digest);
        }
    }
    // 鍵長は length_bits / 8（V2/V4 は 16、V1 R2 は 5）
    let key_len = if r == 2 { 5 } else { 16 };
    let key = &digest[..key_len];
    let mut user_pad = o.to_vec();
    if r == 2 {
        user_pad = rc4::rc4(key, &user_pad);
    } else {
        for i in (0u8..=19).rev() {
            let xkey: Vec<u8> = key.iter().map(|b| b ^ i).collect();
            user_pad = rc4::rc4(&xkey, &user_pad);
        }
    }
    Ok(user_pad)
}

// ---------------------------------------------------------------------------
// V5 R6 (AES-256) の鍵生成と認証
// ---------------------------------------------------------------------------

/// PDF 2.0 §7.6.4.3.4 のハッシュアルゴリズム（R6）。`u_bytes` はオーナー側の
/// ときだけ最初の 48 バイトを渡す。
fn r6_hash(password: &[u8], input_hash: &[u8], u_bytes: Option<&[u8]>) -> Vec<u8> {
    let mut k = input_hash.to_vec();
    let mut round: usize = 0;
    loop {
        // k1 = (password || k || u_bytes) を 64 回繰り返す
        let unit_len = password.len() + k.len() + u_bytes.map(|x| x.len()).unwrap_or(0);
        let mut k1 = Vec::with_capacity(unit_len * 64);
        for _ in 0..64 {
            k1.extend_from_slice(password);
            k1.extend_from_slice(&k);
            if let Some(u) = u_bytes {
                k1.extend_from_slice(u);
            }
        }
        // E = AES-CBC(K1, key=k[0..16], iv=k[16..32]) no padding
        let mut key16 = [0u8; 16];
        key16.copy_from_slice(&k[0..16]);
        let mut iv16 = [0u8; 16];
        iv16.copy_from_slice(&k[16..32]);
        let e = match aes::aes_cbc_encrypt_nopad(&key16, &iv16, &k1) {
            Ok(v) => v,
            Err(_) => return k, // 安全側のフォールバック（実際には起きない）
        };
        // 最初の 16 バイトの 128bit 値 mod 3 で次のハッシュサイズを決める。
        let sum_mod3 = mod3_first16(&e);
        k = match sum_mod3 {
            0 => sha2::sha256(&e).to_vec(),
            1 => sha2::sha384(&e).to_vec(),
            _ => sha2::sha512(&e).to_vec(),
        };
        // 終了条件: PDF 2.0 §7.6.4.3.4 は「64 ラウンド以降、E の最後のバイトが
        // ラウンド番号 - 32 以下なら終了」と書くが、Acrobat/mupdf は 1-indexed
        // のラウンド番号で実装する（=「すでに 64 回以上イテレーションした上で、
        // 最後の E の末尾バイト ≤ そのラウンド番号 - 32」）。本変数 `round` は
        // 0-indexed なので +1 して比較する。
        let last_byte = *e.last().unwrap_or(&0) as usize;
        let one_indexed = round + 1;
        if one_indexed >= 64 && last_byte <= one_indexed.saturating_sub(32) {
            return k[..32].to_vec();
        }
        round += 1;
        if round > 1024 {
            // 異常入力時の保険。仕様通りの入力なら 64〜数百で収束する。
            return k[..32].to_vec();
        }
    }
}

/// 先頭 16 バイトを 128bit 整数とみなして mod 3 を計算する。
fn mod3_first16(bytes: &[u8]) -> u8 {
    // 256 mod 3 = 1 なので、バイト和の mod 3 と等しい。
    let mut acc: u32 = 0;
    for &b in bytes.iter().take(16) {
        acc = (acc + b as u32) % 3;
    }
    acc as u8
}

fn authenticate_r6(password: &[u8], u: &[u8], ue: &[u8], o: &[u8], oe: &[u8]) -> Result<Vec<u8>> {
    if u.len() < 48 || ue.len() < 32 {
        return Err(PdfError::Invalid(
            "V5/R6 /U or /UE has unexpected length".into(),
        ));
    }
    let u_hash = &u[..32];
    let u_valid_salt = &u[32..40];
    let u_key_salt = &u[40..48];
    let o_hash = &o[..32];
    let o_valid_salt = &o[32..40];
    let o_key_salt = &o[40..48];
    let pw = if password.len() > 127 {
        &password[..127]
    } else {
        password
    };

    // a) Owner password を試す（仕様の順序は所有者 → ユーザー だが、空 PW
    //    のユーザー検証が成功する PDF が多いのでユーザー側を先に試す）。
    let mut input = Vec::with_capacity(pw.len() + 8);
    input.extend_from_slice(pw);
    input.extend_from_slice(u_valid_salt);
    let h = r6_hash(pw, &sha2::sha256(&input), None);
    if &h[..32] == u_hash {
        // ユーザーパスワード一致 → /UE から鍵を作る。
        let mut input2 = Vec::with_capacity(pw.len() + 8);
        input2.extend_from_slice(pw);
        input2.extend_from_slice(u_key_salt);
        let intermediate = r6_hash(pw, &sha2::sha256(&input2), None);
        let key = decrypt_file_key_r6(&intermediate, ue)?;
        return Ok(key);
    }

    // b) 所有者パスワードとして試す。
    let mut input = Vec::with_capacity(pw.len() + 8 + 48);
    input.extend_from_slice(pw);
    input.extend_from_slice(o_valid_salt);
    input.extend_from_slice(&u[..48]);
    let h = r6_hash(pw, &sha2::sha256(&input), Some(&u[..48]));
    if &h[..32] == o_hash {
        let mut input2 = Vec::with_capacity(pw.len() + 8 + 48);
        input2.extend_from_slice(pw);
        input2.extend_from_slice(o_key_salt);
        input2.extend_from_slice(&u[..48]);
        let intermediate = r6_hash(pw, &sha2::sha256(&input2), Some(&u[..48]));
        let key = decrypt_file_key_r6(&intermediate, oe)?;
        return Ok(key);
    }

    Err(PdfError::Invalid(
        "PDF requires a non-empty password to decrypt".into(),
    ))
}

fn decrypt_file_key_r6(intermediate: &[u8], encrypted_key: &[u8]) -> Result<Vec<u8>> {
    if intermediate.len() < 32 || encrypted_key.len() < 32 {
        return Err(PdfError::Invalid("V5/R6 intermediate key too short".into()));
    }
    let iv = [0u8; 16];
    let out = aes::aes_cbc_decrypt_nopad(&intermediate[..32], &iv, &encrypted_key[..32])?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------

fn get_int(dict: &Dictionary, key: &str, resolve: &impl Fn(&Object) -> Object) -> Option<i64> {
    let o = dict.get(key)?;
    let r = match o {
        Object::Reference(_) => resolve(o),
        other => other.clone(),
    };
    r.as_int().ok()
}

fn get_string(
    dict: &Dictionary,
    key: &str,
    resolve: &impl Fn(&Object) -> Object,
) -> Option<Vec<u8>> {
    let o = dict.get(key)?;
    let r = match o {
        Object::Reference(_) => resolve(o),
        other => other.clone(),
    };
    match r {
        Object::String(b, _) => Some(b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_password_short() {
        let p = pad_password(b"hi");
        assert_eq!(&p[..2], b"hi");
        assert_eq!(p[2], PAD[0]);
        assert_eq!(p[31], PAD[29]);
    }

    #[test]
    fn pad_password_long() {
        let long = vec![b'x'; 64];
        let p = pad_password(&long);
        assert_eq!(p.to_vec(), vec![b'x'; 32]);
    }

    #[test]
    fn object_key_v2_format() {
        // R3 / 128bit、空パスワードで作った想定の合成テスト。
        // file_key が 16 バイトのとき、object key = MD5(file_key + n3 + g2)[..16]
        let handler = StandardHandler {
            file_key: vec![0xaa; 16],
            v: 2,
            r: 3,
            str_method: CryptMethod::Rc4,
            stm_method: CryptMethod::Rc4,
        };
        let k_rc4 = handler.object_key((123, 0), false);
        assert_eq!(k_rc4.len(), 16);
        let k_aes = handler.object_key((123, 0), true);
        assert_eq!(k_aes.len(), 16);
        assert_ne!(k_rc4, k_aes);
    }
}

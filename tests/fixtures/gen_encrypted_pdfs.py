#!/usr/bin/env python3
"""暗号化 PDF テストフィクスチャを PyMuPDF で生成する。

各リビジョンで「Hello, Encrypted PDF!」を含む 1 ページの PDF を、
ユーザーパスワード = "" / オーナーパスワード = "owner-secret" で保護して保存する。

実行: `python gen_encrypted_pdfs.py`（カレントを tests/fixtures に置く）
"""
import os
import sys

try:
    import fitz  # PyMuPDF
except ImportError:
    print("PyMuPDF not installed. `pip install pymupdf`", file=sys.stderr)
    sys.exit(1)

HERE = os.path.dirname(os.path.abspath(__file__))
TEXT = "Hello, Encrypted PDF!"


def build_plain() -> bytes:
    doc = fitz.open()
    page = doc.new_page(width=612, height=792)  # US Letter
    page.insert_text((72, 720), TEXT, fontname="Helvetica", fontsize=16)
    return doc.tobytes()


def encrypt(plain: bytes, encryption: int, name: str):
    doc = fitz.open(stream=plain, filetype="pdf")
    # owner_pw を設定するとセキュリティが有効化される。user_pw は空のまま。
    out = doc.tobytes(
        encryption=encryption,
        owner_pw="owner-secret",
        user_pw="",
        # 全パーミッションを許可（読み取りは P 制限の影響を受けない）
        permissions=int(
            fitz.PDF_PERM_ACCESSIBILITY
            | fitz.PDF_PERM_PRINT
            | fitz.PDF_PERM_COPY
            | fitz.PDF_PERM_ANNOTATE
        ),
    )
    path = os.path.join(HERE, name)
    with open(path, "wb") as f:
        f.write(out)
    print(f"wrote {path} ({len(out)} bytes)")


def main():
    plain = build_plain()
    # 確認用に平文も保存
    with open(os.path.join(HERE, "encrypted_plain.pdf"), "wb") as f:
        f.write(plain)

    # PyMuPDF の定数:
    #   PDF_ENCRYPT_RC4_40   = 2  (V=1, R=2)
    #   PDF_ENCRYPT_RC4_128  = 3  (V=2, R=3)
    #   PDF_ENCRYPT_AES_128  = 4  (V=4, R=4, AESV2)
    #   PDF_ENCRYPT_AES_256  = 6  (V=5, R=6, AESV3)
    encrypt(plain, fitz.PDF_ENCRYPT_RC4_40, "encrypted_rc4_40.pdf")
    encrypt(plain, fitz.PDF_ENCRYPT_RC4_128, "encrypted_rc4_128.pdf")
    encrypt(plain, fitz.PDF_ENCRYPT_AES_128, "encrypted_aes_128.pdf")
    encrypt(plain, fitz.PDF_ENCRYPT_AES_256, "encrypted_aes_256.pdf")


if __name__ == "__main__":
    main()

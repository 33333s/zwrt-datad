#ifndef ZWRT_DATAD_WEB_CRYPTO_H
#define ZWRT_DATAD_WEB_CRYPTO_H

#include <stddef.h>

/* Load the device OpenSSL provider when available. Safe to call repeatedly. */
int web_crypto_init(void);

/*
 * Generate and retain an AES-256 key, then wrap its lowercase hex text with
 * the vendor RSA public key. The returned value is base64 ciphertext suitable
 * for zwrt_web.web_http_enstr_set.
 */
int web_crypto_prepare_registration(const char *public_key_pem,
                                    char *enstr, size_t enstr_len);
int web_crypto_session_ready(void);

/* Drop the retained session key without unloading libcrypto. */
void web_crypto_reset(void);

/*
 * Decrypt Base64(12-byte nonce || 16-byte tag || ciphertext).
 * Returns 1 for a decrypted envelope, 0 for a plaintext/non-envelope value,
 * and -1 when a value looks like an envelope but authentication fails.
 */
int web_crypto_decrypt_envelope(const char *value, char *out, size_t outlen);

/*
 * Encrypt plaintext as Base64(12-byte nonce || 16-byte tag || ciphertext)
 * with the currently registered vendor Web AES-256-GCM session.
 */
int web_crypto_encrypt_envelope(const char *value, char *out, size_t outlen);

#endif

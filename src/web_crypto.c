#include "web_crypto.h"

#include <ctype.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct bio_st BIO;
typedef struct evp_pkey_st EVP_PKEY;
typedef struct evp_pkey_ctx_st EVP_PKEY_CTX;
typedef struct evp_cipher_st EVP_CIPHER;
typedef struct evp_cipher_ctx_st EVP_CIPHER_CTX;

#define EVP_CTRL_AEAD_SET_IVLEN 0x9
#define EVP_CTRL_AEAD_SET_TAG 0x11
#define RSA_PKCS1_PADDING 1

struct crypto_api {
    void *handle;
    BIO *(*BIO_new_mem_buf)(const void *, int);
    int (*BIO_free)(BIO *);
    EVP_PKEY *(*PEM_read_bio_PUBKEY)(BIO *, EVP_PKEY **, void *, void *);
    void (*EVP_PKEY_free)(EVP_PKEY *);
    EVP_PKEY_CTX *(*EVP_PKEY_CTX_new)(EVP_PKEY *, void *);
    void (*EVP_PKEY_CTX_free)(EVP_PKEY_CTX *);
    int (*EVP_PKEY_encrypt_init)(EVP_PKEY_CTX *);
    int (*EVP_PKEY_CTX_set_rsa_padding)(EVP_PKEY_CTX *, int);
    int (*EVP_PKEY_encrypt)(EVP_PKEY_CTX *, unsigned char *, size_t *,
                            const unsigned char *, size_t);
    int (*RAND_bytes)(unsigned char *, int);
    EVP_CIPHER_CTX *(*EVP_CIPHER_CTX_new)(void);
    void (*EVP_CIPHER_CTX_free)(EVP_CIPHER_CTX *);
    const EVP_CIPHER *(*EVP_aes_256_gcm)(void);
    int (*EVP_DecryptInit_ex)(EVP_CIPHER_CTX *, const EVP_CIPHER *, void *,
                              const unsigned char *, const unsigned char *);
    int (*EVP_CIPHER_CTX_ctrl)(EVP_CIPHER_CTX *, int, int, void *);
    int (*EVP_DecryptUpdate)(EVP_CIPHER_CTX *, unsigned char *, int *,
                             const unsigned char *, int);
    int (*EVP_DecryptFinal_ex)(EVP_CIPHER_CTX *, unsigned char *, int *);
};

static struct crypto_api api;
static unsigned char session_key[32];
static int session_key_valid;

static int load_symbol(void **target, const char *name)
{
    *target = dlsym(api.handle, name);
    return *target != NULL;
}

int web_crypto_init(void)
{
    static const char *const candidates[] = {
        "/usr/lib/libcrypto.so.3", "libcrypto.so.3", "libcrypto.so.1.1"
    };
    if (api.handle) return 1;
    for (size_t i = 0; i < sizeof candidates / sizeof candidates[0]; i++) {
        api.handle = dlopen(candidates[i], RTLD_NOW | RTLD_LOCAL);
        if (api.handle) break;
    }
    if (!api.handle) return 0;

#define LOAD(name) do { if (!load_symbol((void **)&api.name, #name)) goto fail; } while (0)
    LOAD(BIO_new_mem_buf);
    LOAD(BIO_free);
    LOAD(PEM_read_bio_PUBKEY);
    LOAD(EVP_PKEY_free);
    LOAD(EVP_PKEY_CTX_new);
    LOAD(EVP_PKEY_CTX_free);
    LOAD(EVP_PKEY_encrypt_init);
    LOAD(EVP_PKEY_CTX_set_rsa_padding);
    LOAD(EVP_PKEY_encrypt);
    LOAD(RAND_bytes);
    LOAD(EVP_CIPHER_CTX_new);
    LOAD(EVP_CIPHER_CTX_free);
    LOAD(EVP_aes_256_gcm);
    LOAD(EVP_DecryptInit_ex);
    LOAD(EVP_CIPHER_CTX_ctrl);
    LOAD(EVP_DecryptUpdate);
    LOAD(EVP_DecryptFinal_ex);
#undef LOAD
    return 1;

fail:
    dlclose(api.handle);
    memset(&api, 0, sizeof api);
    return 0;
}

static size_t base64_encode(const unsigned char *src, size_t len,
                            char *out, size_t outlen)
{
    static const char table[] =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    size_t need = ((len + 2) / 3) * 4;
    size_t i = 0, pos = 0;
    if (outlen <= need) return 0;
    while (i < len) {
        uint32_t v = (uint32_t)src[i++] << 16;
        int remain = (int)(len - (i - 1));
        if (i < len) v |= (uint32_t)src[i++] << 8;
        if (i < len) v |= src[i++];
        out[pos++] = table[(v >> 18) & 63];
        out[pos++] = table[(v >> 12) & 63];
        out[pos++] = remain > 1 ? table[(v >> 6) & 63] : '=';
        out[pos++] = remain > 2 ? table[v & 63] : '=';
    }
    out[pos] = 0;
    return pos;
}

static int base64_value(unsigned char c)
{
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}

static size_t base64_decode(const char *src, unsigned char *out, size_t outlen)
{
    uint32_t value = 0;
    int bits = 0;
    size_t pos = 0;
    for (const unsigned char *p = (const unsigned char *)src; p && *p; p++) {
        int digit;
        if (*p == '=') break;
        if (isspace(*p)) continue;
        digit = base64_value(*p);
        if (digit < 0) return 0;
        value = (value << 6) | (uint32_t)digit;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            if (pos >= outlen) return 0;
            out[pos++] = (unsigned char)(value >> bits);
            value &= bits ? ((1u << bits) - 1u) : 0u;
        }
    }
    return pos;
}

static int looks_hex_encoded(const char *value)
{
    size_t length = value ? strlen(value) : 0;
    if (length == 0 || (length % 2) != 0) return 0;
    for (const unsigned char *p = (const unsigned char *)value; *p; p++) {
        if (!isxdigit(*p)) return 0;
    }
    return 1;
}

static int normalize_pem(const char *raw, char *out, size_t outlen)
{
    static const char *const markers[] = {"PUBLIC KEY", "RSA PUBLIC KEY"};
    for (size_t m = 0; m < sizeof markers / sizeof markers[0]; m++) {
        char begin[64], end[64];
        const char *start, *finish, *body;
        size_t pos = 0, column = 0;
        snprintf(begin, sizeof begin, "-----BEGIN %s-----", markers[m]);
        snprintf(end, sizeof end, "-----END %s-----", markers[m]);
        start = strstr(raw ? raw : "", begin);
        finish = strstr(raw ? raw : "", end);
        if (!start || !finish || finish <= start) continue;
        if (snprintf(out, outlen, "%s\n", begin) >= (int)outlen) return 0;
        pos = strlen(out);
        body = start + strlen(begin);
        while (body < finish) {
            unsigned char c = (unsigned char)*body++;
            if (isspace(c)) continue;
            if (pos + 2 >= outlen) return 0;
            out[pos++] = (char)c;
            if (++column == 64) {
                out[pos++] = '\n';
                column = 0;
            }
        }
        if (column && pos + 1 < outlen) out[pos++] = '\n';
        if (pos + strlen(end) + 2 >= outlen) return 0;
        memcpy(out + pos, end, strlen(end));
        pos += strlen(end);
        out[pos++] = '\n';
        out[pos] = 0;
        return 1;
    }
    return 0;
}

int web_crypto_prepare_registration(const char *public_key_pem,
                                    char *enstr, size_t enstr_len)
{
    char normalized[8192];
    char key_hex[65];
    unsigned char wrapped[1024];
    size_t wrapped_len = sizeof wrapped;
    BIO *bio = NULL;
    EVP_PKEY *key = NULL;
    EVP_PKEY_CTX *ctx = NULL;
    int ok = 0;

    web_crypto_reset();
    if (!web_crypto_init() || !normalize_pem(public_key_pem, normalized, sizeof normalized))
        return 0;
    bio = api.BIO_new_mem_buf(normalized, (int)strlen(normalized));
    if (!bio) goto done;
    key = api.PEM_read_bio_PUBKEY(bio, NULL, NULL, NULL);
    if (!key) goto done;
    if (api.RAND_bytes(session_key, sizeof session_key) != 1) goto done;
    for (size_t i = 0; i < sizeof session_key; i++)
        snprintf(key_hex + i * 2, 3, "%02x", session_key[i]);
    key_hex[64] = 0;

    ctx = api.EVP_PKEY_CTX_new(key, NULL);
    if (!ctx || api.EVP_PKEY_encrypt_init(ctx) <= 0 ||
        api.EVP_PKEY_CTX_set_rsa_padding(ctx, RSA_PKCS1_PADDING) <= 0)
        goto done;
    if (api.EVP_PKEY_encrypt(ctx, NULL, &wrapped_len,
                             (const unsigned char *)key_hex, 64) <= 0 ||
        wrapped_len > sizeof wrapped)
        goto done;
    if (api.EVP_PKEY_encrypt(ctx, wrapped, &wrapped_len,
                             (const unsigned char *)key_hex, 64) <= 0)
        goto done;
    if (!base64_encode(wrapped, wrapped_len, enstr, enstr_len)) goto done;
    session_key_valid = 1;
    ok = 1;

done:
    memset(key_hex, 0, sizeof key_hex);
    memset(wrapped, 0, sizeof wrapped);
    if (ctx) api.EVP_PKEY_CTX_free(ctx);
    if (key) api.EVP_PKEY_free(key);
    if (bio) api.BIO_free(bio);
    if (!ok) web_crypto_reset();
    return ok;
}

void web_crypto_reset(void)
{
    memset(session_key, 0, sizeof session_key);
    session_key_valid = 0;
}

int web_crypto_session_ready(void)
{
    return session_key_valid;
}

int web_crypto_decrypt_envelope(const char *value, char *out, size_t outlen)
{
    size_t value_len = value ? strlen(value) : 0;
    size_t raw_cap = value_len ? (value_len * 3 / 4 + 4) : 0;
    unsigned char *raw = NULL, *plain = NULL;
    size_t raw_len;
    EVP_CIPHER_CTX *ctx = NULL;
    int produced = 0, final_len = 0, ok = -1;

    if (!value || !*value || value_len < 40 || looks_hex_encoded(value)) return 0;
    raw = malloc(raw_cap);
    if (!raw) return -1;
    raw_len = base64_decode(value, raw, raw_cap);
    if (raw_len <= 28) {
        free(raw);
        return 0;
    }
    if (!session_key_valid || !web_crypto_init()) goto done;
    plain = malloc(raw_len - 28 + 1);
    if (!plain) goto done;
    ctx = api.EVP_CIPHER_CTX_new();
    if (!ctx || api.EVP_DecryptInit_ex(ctx, api.EVP_aes_256_gcm(), NULL, NULL, NULL) != 1 ||
        api.EVP_CIPHER_CTX_ctrl(ctx, EVP_CTRL_AEAD_SET_IVLEN, 12, NULL) != 1 ||
        api.EVP_DecryptInit_ex(ctx, NULL, NULL, session_key, raw) != 1 ||
        api.EVP_DecryptUpdate(ctx, plain, &produced, raw + 28, (int)(raw_len - 28)) != 1 ||
        api.EVP_CIPHER_CTX_ctrl(ctx, EVP_CTRL_AEAD_SET_TAG, 16, raw + 12) != 1 ||
        api.EVP_DecryptFinal_ex(ctx, plain + produced, &final_len) != 1)
        goto done;
    if ((size_t)(produced + final_len) >= outlen) goto done;
    memcpy(out, plain, (size_t)(produced + final_len));
    out[produced + final_len] = 0;
    ok = 1;

done:
    if (ctx) api.EVP_CIPHER_CTX_free(ctx);
    if (plain) {
        memset(plain, 0, raw_len - 28 + 1);
        free(plain);
    }
    if (raw) {
        memset(raw, 0, raw_cap);
        free(raw);
    }
    return ok;
}

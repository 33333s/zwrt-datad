#include "web_crypto.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include <openssl/evp.h>
#include <openssl/pem.h>
#include <openssl/rsa.h>
int main(void) {
    EVP_PKEY_CTX *gen = EVP_PKEY_CTX_new_id(EVP_PKEY_RSA, NULL);
    EVP_PKEY *key = NULL;
    BIO *pem = BIO_new(BIO_s_mem());
    char public_key[4096], wrapped[4096], encrypted[4096], decrypted[4096];
    const char *message = "SMS fixture: encrypted text 你好";
    assert(gen && pem && EVP_PKEY_keygen_init(gen) > 0);
    assert(EVP_PKEY_CTX_set_rsa_keygen_bits(gen, 2048) > 0);
    assert(EVP_PKEY_keygen(gen, &key) > 0);
    assert(PEM_write_bio_PUBKEY(pem, key) == 1);
    int n = BIO_read(pem, public_key, sizeof public_key - 1);
    assert(n > 0); public_key[n] = 0;
    assert(web_crypto_init());
    assert(web_crypto_prepare_registration(public_key, wrapped, sizeof wrapped));
    assert(web_crypto_session_ready());
    assert(web_crypto_encrypt_envelope(message, encrypted, sizeof encrypted));
    assert(web_crypto_decrypt_envelope(encrypted, decrypted, sizeof decrypted) == 1);
    assert(strcmp(message, decrypted) == 0);
    encrypted[20] = encrypted[20] == 'A' ? 'B' : 'A';
    assert(web_crypto_decrypt_envelope(encrypted, decrypted, sizeof decrypted) == -1);
    assert(web_crypto_decrypt_envelope("00310032", decrypted, sizeof decrypted) == 0);
    web_crypto_reset();
    assert(!web_crypto_session_ready());
    assert(!web_crypto_encrypt_envelope(message, encrypted, sizeof encrypted));
    BIO_free(pem); EVP_PKEY_free(key); EVP_PKEY_CTX_free(gen);
    puts("RSA registration and authenticated SMS encryption tests passed");
    return 0;
}

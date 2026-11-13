/* ESP-IDF's protocomm Security 2 handler as a stand-alone "device" for the
 * login test (src/login.rs): srp-device SALT_HEX VERIFIER_HEX
 * reads one hex SessionData per line and prints the hex reply, or "ERR". */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "psa/crypto.h"
#include <protocomm_security.h>
#include <protocomm_security2.h>

static size_t unhex(const char *hex, unsigned char *out)
{
    size_t n = strlen(hex) / 2;
    for (size_t i = 0; i < n; i++) sscanf(hex + 2 * i, "%2hhx", &out[i]);
    return n;
}

int main(int argc, char **argv)
{
    static unsigned char salt[64], verifier[512], in[4096];
    static char line[9000];
    if (argc != 3) return 2;
    psa_crypto_init();
    protocomm_security2_params_t params = {
        .salt = (const char *)salt, .salt_len = (uint16_t)unhex(argv[1], salt),
        .verifier = (const char *)verifier, .verifier_len = (uint16_t)unhex(argv[2], verifier),
    };
    protocomm_security_handle_t h = NULL;
    protocomm_security2.init(&h);
    protocomm_security2.new_transport_session(h, 1);
    while (fgets(line, sizeof(line), stdin)) {
        line[strcspn(line, "\r\n")] = 0;
        size_t n = unhex(line, in);
        uint8_t *out = NULL; ssize_t out_len = 0;
        if (protocomm_security2.security_req_handler(h, &params, 1, in, n, &out, &out_len, NULL) != ESP_OK) {
            printf("ERR\n");
        } else {
            for (ssize_t i = 0; i < out_len; i++) printf("%02x", out[i]);
            printf("\n");
            free(out);
        }
        fflush(stdout);
    }
    return 0;
}

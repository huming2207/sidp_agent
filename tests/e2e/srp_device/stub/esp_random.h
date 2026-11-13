#pragma once
#include <stddef.h>
#include <stdint.h>
#include <sys/random.h>
static inline void esp_fill_random(void *buf, size_t len) { getrandom(buf, len, 0); }
static inline uint32_t esp_random(void) { uint32_t r; getrandom(&r, sizeof(r), 0); return r; }

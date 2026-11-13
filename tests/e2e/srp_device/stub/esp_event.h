#pragma once
#include <stddef.h>
#include <stdint.h>
typedef const char *esp_event_base_t;
#define ESP_EVENT_DECLARE_BASE(id) extern esp_event_base_t const id
#define ESP_EVENT_DEFINE_BASE(id) esp_event_base_t const id = #id
#define portMAX_DELAY 0xffffffffu
static inline int esp_event_post(esp_event_base_t b, int32_t id, const void *d, size_t s, uint32_t t) { (void)b; (void)id; (void)d; (void)s; (void)t; return 0; }

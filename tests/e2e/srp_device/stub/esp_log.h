#pragma once
#include <stdio.h>
#define ESP_LOGE(tag, fmt, ...) fprintf(stderr, "E %s: " fmt "\n", tag, ##__VA_ARGS__)
#define ESP_LOGW(tag, fmt, ...) do {} while (0)
#define ESP_LOGI(tag, fmt, ...) do {} while (0)
#define ESP_LOGD(tag, fmt, ...) do {} while (0)
#define ESP_LOGV(tag, fmt, ...) do {} while (0)
#define ESP_LOG_BUFFER_HEX_LEVEL(...) do {} while (0)
#define ESP_LOG_BUFFER_HEXDUMP(...) do {} while (0)
#define ESP_LOG_DEBUG 0
#define ESP_LOG_VERBOSE 0

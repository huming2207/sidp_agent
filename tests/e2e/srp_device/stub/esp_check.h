#pragma once
#include "esp_log.h"
#define ESP_RETURN_ON_FALSE(a, err_code, log_tag, format, ...) do { if (!(a)) { ESP_LOGE(log_tag, format, ##__VA_ARGS__); return err_code; } } while (0)
#define ESP_GOTO_ON_FALSE(a, err_code, goto_tag, log_tag, format, ...) do { if (!(a)) { ESP_LOGE(log_tag, format, ##__VA_ARGS__); ret = err_code; goto goto_tag; } } while (0)
#define ESP_RETURN_ON_ERROR(x, log_tag, format, ...) do { esp_err_t err_rc_ = (x); if (err_rc_ != ESP_OK) { return err_rc_; } } while (0)
#define ESP_GOTO_ON_ERROR(x, goto_tag, log_tag, format, ...) do { esp_err_t err_rc_ = (x); if (err_rc_ != ESP_OK) { ret = err_rc_; goto goto_tag; } } while (0)

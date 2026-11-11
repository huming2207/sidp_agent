#pragma once
// Deterministic Cortex-M register/RAM model, not an instruction-set emulator.
// The real sidp_session still owns protocol validation, snapshots and run state.
#include "sidp_backend.hpp"
#include <algorithm>
#include <array>
#include <chrono>
#include <cstring>
#include <vector>

class MockBackend final : public sidp::target_backend_t {
public:
    explicit MockBackend(bool fpu, int profile) : fpu_(fpu), profile_(profile) {
        regs_[13] = 0x20000200;
        regs_[15] = 0x08000000;
        regs_[16] = 0x01000000;
        regs_[17] = regs_[13];
        regs_[0x40] = 0x3f800000;
        regs_[0x41] = 0x40000000;
        ram_[0x100] = 0x78; ram_[0x101] = 0x56; ram_[0x102] = 0x34; ram_[0x103] = 0x12;
        ram_[0x200] = 0x42;
        for (size_t n = 0; n < flash_.size(); n += 2) { flash_[n] = 0; flash_[n+1] = 0xbf; }
    }
    esp_err_t attach(const sidp::attach_params_t &, sidp::attach_info_t &info) override {
        info.profile = static_cast<sidp::target_profile_t>(profile_);
        info.capabilities = static_cast<sidp::capability_t>(sidp::CAP_STOP_SNAPSHOT | sidp::CAP_HARDWARE_BP |
            sidp::CAP_SINGLE_STEP | sidp::CAP_RESET_HALT | sidp::CAP_RESET_SYSTEM | sidp::CAP_RESET_NRST | (fpu_ ? static_cast<uint32_t>(sidp::CAP_FPU) : 0u));
        info.hardware_breakpoints = 2;
        info.max_memory_transfer = 64; // force real multi-frame reads/writes
        info.memory_regions = {reinterpret_cast<const uint8_t *>(regions_.data()), sizeof(regions_)};
        return ESP_OK;
    }
    esp_err_t detach(sidp::detach_action_t action) override {
        halted_ = action == sidp::DETACH_KEEP_HALTED;
        bps_.clear(); return ESP_OK;
    }
    esp_err_t reset(sidp::reset_kind_t, bool halt_after, sidp::stop_detect_t &stop) override {
        regs_[15] = 0x08000000; halted_ = halt_after; stepping_ = false; stopping_ = false;
        observe(stop); return ESP_OK;
    }
    esp_err_t halt(uint32_t, sidp::stop_detect_t &stop) override {
        halted_ = true; stepping_ = false; stopping_ = false;
        observe(stop); return ESP_OK;
    }
    esp_err_t poll_halted(sidp::stop_detect_t &stop) override {
        if (!halted_ && stopping_ && std::chrono::steady_clock::now() >= stop_at_) {
            halted_ = true;
            if (stepping_) regs_[15] += 2;
            else if (!bps_.empty()) regs_[15] = static_cast<uint32_t>(bps_[0].address);
        }
        observe(stop); return ESP_OK;
    }
    esp_err_t resume(sidp::run_action_t action, uint64_t) override {
        halted_ = false; stepping_ = action == sidp::RUN_SINGLE_STEP;
        stopping_ = stepping_ || !bps_.empty();
        stop_at_ = std::chrono::steady_clock::now() + std::chrono::milliseconds(10);
        return ESP_OK;
    }
    esp_err_t read_regs(std::span<const sidp::register_id_t> ids, std::span<uint8_t> out, size_t &size) override {
        std::vector<sidp::register_id_t> selected(ids.begin(), ids.end());
        if (selected.empty()) {
            for (int n = 0; n <= 22; ++n) if (supported(n)) selected.push_back(static_cast<sidp::register_id_t>(n));
            if (fpu_) for (int n = 0x40; n <= 0x60; ++n) selected.push_back(static_cast<sidp::register_id_t>(n));
        }
        if (out.size() < selected.size() * 8) return ESP_ERR_INVALID_ARG;
        size = 0;
        for (auto id : selected) {
            if (!supported(id)) return ESP_ERR_INVALID_ARG;
            uint16_t raw = id; std::memcpy(out.data()+size, &raw, 2);
            out[size+2] = 4; out[size+3] = 0;
            std::memcpy(out.data()+size+4, &regs_[id], 4); size += 8;
        }
        return ESP_OK;
    }
    esp_err_t write_regs(const uint8_t *data, size_t size) override {
        if (size % 8) return ESP_ERR_INVALID_ARG;
        for (size_t n = 0; n < size; n += 8) {
            uint16_t id; std::memcpy(&id, data+n, 2);
            if (!supported(id) || data[n+2] != 4 || data[n+3] != 0) return ESP_ERR_INVALID_ARG;
        }
        for (size_t n = 0; n < size; n += 8) {
            uint16_t id; std::memcpy(&id, data+n, 2); std::memcpy(&regs_[id], data+n+4, 4);
        }
        return ESP_OK;
    }
    esp_err_t read_mem(uint64_t address, uint8_t *out, size_t size, sidp::memory_access_width_t) override {
        const uint8_t *p = bytes(address,size); if (!p) return ESP_ERR_INVALID_ARG;
        std::memcpy(out,p,size); return ESP_OK;
    }
    esp_err_t write_mem(uint64_t address, const uint8_t *data, size_t size, sidp::memory_access_width_t) override {
        if (address < 0x20000000 || address > 0x20001000 || size > 0x20001000-address) return ESP_ERR_INVALID_ARG;
        std::memcpy(ram_.data()+address-0x20000000,data,size); return ESP_OK;
    }
    esp_err_t apply_breakpoints(std::span<const sidp::bp_entry_t> entries) override {
        if (entries.size() > 2) return ESP_ERR_INVALID_ARG;
        bps_.assign(entries.begin(),entries.end()); return ESP_OK;
    }
    esp_err_t apply_watchpoints(std::span<const sidp::wp_entry_t> entries) override { return entries.empty() ? ESP_OK : ESP_ERR_NOT_SUPPORTED; }
    esp_err_t set_vector_catch(sidp::vector_catch_t mask) override { return mask == 0 ? ESP_OK : ESP_ERR_NOT_SUPPORTED; }
private:
    bool supported(int id) const {
        return (id >= 0 && id <= 22 && !(profile_ == 1 && (id == 20 || id == 21))) || (fpu_ && id >= 0x40 && id <= 0x60);
    }
    void observe(sidp::stop_detect_t &stop) {
        stop = {}; stop.halted = halted_; stop.pc = regs_[15];
        if (halted_ && stepping_) stop.dfsr = 1;
        if (halted_ && stopping_ && !stepping_ && !bps_.empty()) { stop.comparator_match=true; stop.dfsr=2; }
    }
    const uint8_t *bytes(uint64_t address, size_t size) const {
        if (address >= 0x20000000 && address <= 0x20001000 && size <= 0x20001000-address) return ram_.data()+address-0x20000000;
        if (address >= 0x08000000 && address <= 0x08001000 && size <= 0x08001000-address) return flash_.data()+address-0x08000000;
        return nullptr;
    }
    bool fpu_, halted_ = false, stepping_ = false, stopping_ = false;
    int profile_;
    std::chrono::steady_clock::time_point stop_at_;
    std::array<uint32_t, 0x61> regs_{};
    std::array<uint8_t,4096> ram_{},flash_{};
    std::vector<sidp::bp_entry_t> bps_;
    std::array<sidp::memory_region_t,2> regions_{{
        {0x08000000,4096,sidp::MEMORY_FLASH,static_cast<sidp::memory_flag_t>(sidp::MEM_READ|sidp::MEM_EXECUTE),{0,0}},
        {0x20000000,4096,sidp::MEMORY_RAM,static_cast<sidp::memory_flag_t>(sidp::MEM_READ|sidp::MEM_WRITE|sidp::MEM_EXECUTE|sidp::MEM_CACHEABLE),{0,0}}
    }};
};

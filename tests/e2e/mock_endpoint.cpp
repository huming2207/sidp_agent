// SLIP stdin/stdout endpoint using the actual Soul Injector sidp_session.
// The Python runner connects these pipes to a PTY emulating USB CDC byte I/O.
#include "mock_backend.hpp"
#include "sidp_session.hpp"
#include "sidp_transport.hpp"
#include <cerrno>
#include <csignal>
#include <cstdio>
#include <poll.h>
#include <unistd.h>

bool send_frame(std::span<const uint8_t> frame) {
    std::vector<uint8_t> encoded{0xc0};
    for (auto b : frame) {
        if (b == 0xc0) { encoded.push_back(0xdb); encoded.push_back(0xdc); }
        else if (b == 0xdb) { encoded.push_back(0xdb); encoded.push_back(0xdd); }
        else encoded.push_back(b);
    }
    encoded.push_back(0xc0);
    size_t offset = 0;
    while (offset < encoded.size()) {
        auto n = write(STDOUT_FILENO,encoded.data()+offset,encoded.size()-offset);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return false;
        offset += static_cast<size_t>(n);
    }
    return true;
}
int main(int argc,char **argv) {
    signal(SIGPIPE,SIG_IGN);
    const int profile=argc>1 ? std::atoi(argv[1]) : 3;
    MockBackend backend(argc>2 && std::string_view(argv[2])=="fpu",profile);
    sidp::sidp_session session(backend,send_frame);
    if (session.init()!=ESP_OK) return 1;
    std::vector<uint8_t> frame;
    bool started=false,escape=false,discard=false;
    for (;;) {
        pollfd input{STDIN_FILENO,POLLIN,0};
        auto result=poll(&input,1,2);
        if (result<0 && errno==EINTR) continue;
        if (result<0) return 2;
        if (result>0) {
            uint8_t buf[512];auto size=read(STDIN_FILENO,buf,sizeof(buf));
            if (size<=0) break;
            for (ssize_t n=0;n<size;++n) {
                uint8_t b=buf[n];
                if (b==0xc0) {
                    if (started && !escape && !discard && !frame.empty()) {
                        if (!sidp::crc32_hasher::verify_message_crc(frame)) return 3;
                        if (frame.size()<12 || frame.size()>sidp::MAX_FRAME_SIZE) return 4;
                        auto *header=reinterpret_cast<const sidp::msg_header_t*>(frame.data());
                        fprintf(stderr,"REQUEST %u\n",header->opcode);
                        session.handle_request(frame);
                    }
                    frame.clear();started=true;escape=false;discard=false;continue;
                }
                if (!started || discard) continue;
                if (escape) {
                    escape=false;
                    if (b==0xdc) b=0xc0;else if (b==0xdd) b=0xdb;else {discard=true;continue;}
                } else if (b==0xdb) {escape=true;continue;}
                if (frame.size()==sidp::MAX_FRAME_SIZE) discard=true;else frame.push_back(b);
            }
        }
        session.handle_poll();
        if (session.needs_disconnect()) break;
    }
    return session.handle_disconnect() ? 0 : 5;
}

// Soul Injector agent session API: what scripts can call.
//
// eval: a script (no import/export) with these globals: target, flash, crypto,
// agent, console. Its value is the last statement's; top-level await works.
// run_script: an ES module that imports the same things from 'si:target',
// 'si:flash', 'si:crypto' and 'si:agent'.
// There are no timers (setTimeout/setInterval), no fetch, no files: wait with
// `await target.delay(ms)` or `target.poll32(...)`.

// Plain arrays are refused (TypeError): a number per byte costs the probe far
// more RAM. Write `new Uint8Array([0x01, 0x02])`.
type Bytes = Uint8Array | ArrayBuffer;

/** Sync calls block the script until the probe answers. */
interface SyncOptions {
    timeoutMs?: number; // default 500
    retries?: number;   // default 0
}

/** Thrown, or rejected with, by every target call. */
interface TargetError extends Error {
    code:
        | 'NOT_ALLOWED'  // needs a permission this session doesn't have (agent.permissions())
        | 'BUSY'         // a sync call while call(), reset(), reinit() or flash.* is pending, >4 promise calls,
                         // a second buffer call (readMem, writeMem, flash.*) before the first ended,
                         // or a second watch of the same kind
        | 'TIMEOUT'
        | 'STOPPED'      // the runtime is being stopped
        | 'NOT_HALTED'   // readReg/writeReg while the core runs
        | 'UNSUPPORTED'
        | 'FAULT';       // the SWD operation failed (bad address, target gone...)
    err: number;         // esp_err_t
}

type Reg =
    | 'r0' | 'r1' | 'r2' | 'r3' | 'r4' | 'r5' | 'r6' | 'r7' | 'r8' | 'r9' | 'r10' | 'r11' | 'r12'
    | 'sp' | 'lr' | 'pc' | 'xpsr' | 'msp' | 'psp' | 'fpscr'
    | `s${number}`  // s0-s31; these and fpscr are UNSUPPORTED without an FPU
    | number;       // DCRSR REGSEL 0-127; 20 is CONTROL<<24 | FAULTMASK<<16 | BASEPRI<<8 | PRIMASK

declare namespace target {
    function read32(addr: number, opts?: SyncOptions): number;
    /** Needs write. */
    function write32(addr: number, value: number, opts?: SyncOptions): void;
    /** Writes and returns (old & andMask) | orBits. Needs write. */
    function rmw32(addr: number, andMask: number, orBits: number, opts?: SyncOptions): number;
    function halt(opts?: SyncOptions): void;
    function resume(opts?: SyncOptions): void;
    /** The core must be halted. */
    function readReg(reg: Reg, opts?: SyncOptions): number;
    /** The core must be halted. Needs write. */
    function writeReg(reg: Reg, value: number, opts?: SyncOptions): void;
    /** At most 4096 bytes. Works while the core runs. */
    function readMem(addr: number, len: number, opts?: SyncOptions): Uint8Array;
    /** At most 4096 bytes. Needs write. */
    function writeMem(addr: number, bytes: Bytes, opts?: SyncOptions): void;
    /** Resolves with the value once (value & mask) === expect. */
    function poll32(addr: number, mask: number, expect: number,
                    opts?: { timeoutMs?: number; intervalMs?: number }): Promise<number>;
    /** Runs the function at addr in target RAM with interrupts masked; resolves with r0.
     *  stackTop: 8-byte aligned, inside a RAM region of the probe's job. Needs write. */
    function call(addr: number, args: [number?, number?, number?, number?],
                  opts: { stackTop: number; timeoutMs?: number }): Promise<number>;
    /** Resolves when the core halts; TIMEOUT after timeoutMs (default 1000). */
    function waitHalt(opts?: { timeoutMs?: number }): Promise<void>;
    /** Pulses the reset line; the target then runs. Needs reset. */
    function reset(): Promise<void>;
    /** Reconnects SWD, e.g. after the target changed its debug settings. Needs reset. */
    function reinit(): Promise<void>;
    function delay(ms: number): Promise<void>;
    /** Calls handler (if any) whenever the target does `on`. One watch per kind;
     *  they end with the runtime (run_script, stop_script, or an eval after it ended). */
    function watch(spec: WatchSpec, handler?: WatchHandler): Watch;
}

declare namespace flash {
    /** Through the job's flash algorithm, inside its flash range. Erase and write reset the
     *  target first. Outside the flash, length/count 0 or misaligned: RangeError, target untouched. */
    function read(addr: number, len: number): Promise<Uint8Array>;
    /** count sectors (the job's sector map, else page_size), the first starting at addr. Needs flash. */
    function erase(addr: number, count: number): Promise<void>;
    /** Erased flash, at most 4096 bytes, a length the flash can program. Needs flash. */
    function write(addr: number, bytes: Bytes): Promise<void>;
}

declare namespace crypto {
    function randomBytes(n: number): Uint8Array;   // at most 4096
    function sha256(data: Bytes | string): Uint8Array;
}

declare namespace agent {
    /** An 'emit' event {name, data}. name: 1-31 bytes; data's JSON: about 700 bytes at most.
     *  false when rate limited (console and emit share 20 a second, bursts of 40). */
    function emit(name: string, data?: unknown): boolean;
    /** In a trap: records an incident on the probe and resolves with its id (0 when the trap's
     *  incidents_per_hour ran out). In a session: only an 'incident' event, id 0.
     *  name: 1-23 bytes; data's JSON: at most 8191 bytes. Await it before the next one. */
    function incident(name: string, data?: unknown): Promise<number>;
    function watches(): WatchInfo[];
    function permissions(): { write: boolean; flash: boolean; reset: boolean };
}

interface WatchSpec {
    /** fault: HardFault/MemManage/BusFault/UsageFault, caught at the vector (ARMv6-M: HardFault only).
     *  halt: any halt this session didn't ask for: breakpoint, watchpoint, external.
     *  lockup: the core locked up. reset: the core reset. */
    on: 'fault' | 'halt' | 'lockup' | 'reset';
    /** What the probe does at once, before the handler runs:
     *  halt (default; reset watches default to none): keep or put the core halted, so the
     *  handler sees the stop and its result decides; none: let it run on; reset: reset it
     *  (needs reset; not for reset watches). */
    reflex?: 'halt' | 'none' | 'reset';
    name?: string;            // at most 23 bytes, shown in events
    handlerTimeoutMs?: number; // 1-60000, default 5000; past it the core stays halted
    capture?: {
        regs?: boolean;       // default true (only read while halted)
        fault?: boolean;      // SHCSR CFSR HFSR DFSR MMFAR BFAR AFSR; default true for fault and lockup
        stack?: number;       // bytes from the stacked frame's SP: default 256 (0 for reset), max 4096
        mem?: [number, number][]; // up to 4 [addr, len], word aligned, len <= 4096
    };                        // stack + mem: at most 12288 bytes
}

/** The returned verdict counts when the core was held (ev.held): 'hold' (or undefined)
 *  keeps it halted, 'resume' lets it run, 'reset' resets it (needs reset).
 *  In a trap, { action, data } also puts data (JSON, at most 8191 bytes) in the incident. */
type WatchHandler = (ev: WatchEvent) => Verdict | HandlerResult | Promise<Verdict | HandlerResult>;
type Verdict = 'hold' | 'resume' | 'reset' | undefined;
interface HandlerResult { action?: Verdict; data?: unknown }

interface WatchEvent {
    watch: number;
    on: 'fault' | 'halt' | 'lockup' | 'reset';
    name: string;
    cause: string;            // HardFault, MemManage, BusFault, UsageFault, breakpoint,
                              // watchpoint, external, halt request, lockup, reset
    reflex: 'halt' | 'none' | 'reset';
    halted: boolean;          // the core was halted for the capture
    held: boolean;            // the core waits for this handler's verdict
    pc: number;               // 0 unless halted
    captureId: number;        // get_capture; 0 when every capture slot was busy
    incidentId: number;       // in a trap, the incident this firing recorded; else 0
    regs?: Record<string, number>; // r0-r12 sp lr pc xpsr msp psp primask basepri faultmask control
    fault?: { shcsr?: number; cfsr?: number; hfsr?: number; dfsr?: number;
              mmfar?: number; bfar?: number; afsr?: number };
    stack?: { addr: number; data: Uint8Array };
    mem: { addr: number; data: Uint8Array }[];
}

interface WatchInfo {
    id: number;
    on: 'fault' | 'halt' | 'lockup' | 'reset';
    reflex: 'halt' | 'none' | 'reset';
    name: string;
    handler: boolean;
    capture: { regs: boolean; fault: boolean; stack: number; mem: [number, number][] };
}

interface Watch extends WatchInfo {
    /** Removes the watch; false when it was already gone. */
    cancel(): boolean;
}

declare const console: {
    log(...args: unknown[]): void;   // each line becomes a 'console' event
    info(...args: unknown[]): void;
    warn(...args: unknown[]): void;
    error(...args: unknown[]): void;
};

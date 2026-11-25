# Driving a target through Soul Injector

A Soul Injector is a debug probe for Cortex-M targets. These tools run
JavaScript **on the probe**, next to the target, so a script can react to the
target within milliseconds, even when you are thinking. The script API is in
`si://api.d.ts`.

## Sessions

The first tool that needs the target opens an *agent session*. The probe then
owns the target: GDB and the probe's programming jobs can't have it until the
session closes (`close_session`, or this server exiting). Opening one fails
with "busy" while GDB is attached or a job runs.

The person running this server chose the permissions (`device_info` shows
them). Reading memory and registers, halting and resuming are always allowed.
Writing memory or registers and `target.call()` need **write**; `flash.erase`
and `flash.write` need **flash**; `target.reset()`, `reinit()` and 'reset'
reflexes need **reset**. Without one, the call throws a `TargetError` with
code `NOT_ALLOWED`. Don't look for ways around that; tell the person which
permission the task needs.

If the connection to the probe is lost, the next tool opens a new session
and says so: the old runtime, watches and captures are gone.

## eval and run_script

The session has one JavaScript runtime.

- `eval` runs a snippet in it, starting one if none runs, and returns the
  value of its last statement as JSON. Use it to look and to act:
  `target.read32(0xE000ED00)`, `target.halt(); target.readReg('pc')`.
  `var`s and functions it defines stay for later evals.
- `run_script` **replaces** the runtime with an ES module: the old runtime,
  its watches and its globals end first. Use it for watches and for code that
  keeps going. Anything the module puts on `globalThis` is visible to later
  evals, so a module can collect data that you then read with eval.

Both wait `timeout_ms` (default 5 s). After that the outcome is `pending`: the
code carries on, and its result arrives later as a `result` event.

A script must not spin: one stretch of JavaScript may run at most about
200 ms before the probe stops the runtime (outcome `stopped`). Waiting on the
target doesn't count. Loop with `await target.delay(ms)` between rounds;
there is no setTimeout. Memory is a private arena of a few MB; running out
throws in the script and nothing else is affected.

Sync calls (`read32`, `readMem`, `halt`...) block the script until the probe
answers. Promise calls (`delay`, `poll32`, `waitHalt`, `call`, `flash.*`) let
it do other things; at most four at once, and no sync call while one is
pending (`BUSY`).

## Watches

`target.watch(spec, handler)` makes the probe watch the target for a kind of
event, one watch per kind:

| on | fires when |
| -- | ---------- |
| `fault` | a HardFault, MemManage, BusFault or UsageFault starts (vector catch: the core halts at the handler's first instruction) |
| `halt` | the core halts by itself: a breakpoint, a watchpoint, an external halt |
| `lockup` | the core locks up (a fault inside a fault handler) |
| `reset` | the core resets |

When one fires, the probe at once applies the watch's *reflex* (`halt` keeps
the core halted, `none` lets it run, `reset` resets it), takes a *capture*
(registers, fault registers, stack, chosen memory), then calls the handler
with the event. If the core is held, the handler's return value decides:
`'resume'`, `'reset'` or `'hold'` (also the default, and what happens when
the handler throws or takes longer than `handlerTimeoutMs`). Every firing is
also a `watch` event, so you see it even without a handler. A watch firing
more than 10 times a second is throttled: the core is held and the handler
isn't called.

Halts your own calls cause (`target.halt()`) don't fire watches.

## Events

`wait_event` returns what happened since the events you last saw, waiting
for the first one: `watch` (with a `capture_id`), `stop` (a halt with no
watch on it), `console`, `emit` (your `agent.emit(name, data)`), `error`
(an uncaught exception or unhandled rejection in a script), `result`,
`runtime` (the runtime ended by itself, and why) and `target` (stopped
answering, answering again, reset). The probe keeps the last 64 events;
`missed` says how many were overwritten before you read them. `console` and
`emit` share a budget of 20 a second.

## Captures

`get_capture` decodes a capture: registers, the exception the core was in,
the exception frame the core stacked (the `pc` and `lr` of the code that
faulted), fault registers with each set bit explained, and the stack and
memory bytes as hex. With `--elf` given to the server, code addresses also
get function names (`lookup_symbol` works then too). Only the latest four
captures are kept.

## Recipes

Find out why the target faults:

```js
// run_script
import { watch } from 'si:target';
watch({ on: 'fault', name: 'crash', capture: { stack: 512 } });
```

then `wait_event` with `kinds: ["watch"]`, and `get_capture` its
`capture_id`: look at `stacked.pc`, `fault.cfsr_bits` and `fault.bfar` /
`mmfar`. The core stays halted at the fault handler; `resume` or `reset` it
when done.

Sample a variable every 100 ms without stopping the target:

```js
// run_script
import { readMem, delay } from 'si:target';
import { emit } from 'si:agent';
globalThis.samples = [];
(async () => {
    for (;;) {
        const b = readMem(0x20000100, 4);
        samples.push(new DataView(b.buffer, b.byteOffset, 4).getUint32(0, true));
        if (samples.length > 500) samples.shift();
        await delay(100);
    }
})();
```

then read `samples` with eval when you want them, or `emit` when something
interesting happens.

Stop at a function with the FPB (needs write; `lookup_symbol` for the
address, which must be below 0x20000000):

```js
target.write32(0xE0002000, 3);                                  // FP_CTRL: KEY | ENABLE
target.write32(0xE0002008, (addr & 0x1FFFFFFC) | ((addr & 2) ? 0x80000001 : 0x40000001));
```

with a `halt` watch to handle the stop. Clear the comparator (write 0) when
done.

## Traps

A session lasts as long as this conversation. For a bug that takes hours or
days to show, `arm_trap` leaves a module on the probe instead. It keeps
running with no computer attached, through disconnects and probe restarts,
and each time one of its watches fires, or it calls `agent.incident(name,
data)`, the probe writes an *incident* to its flash: what happened, the
capture, the events just before, and the data the handler returned as
`{ action, data }`. Come back later with `list_incidents` and
`get_incident`.

- Prefer `reflex: 'none'`, or a handler that returns `'resume'`: a trap
  that holds the core stops the target until someone looks. Holding is right
  when you want GDB to land on the fault.
- The module starts from the top after a probe restart, after the target
  stopped answering, and after it yielded to GDB or a programming run. Keep
  what it remembers small and rebuildable; put history in the handler's
  `data`, where the incident keeps it.
- In `yield` mode (default) GDB attaching or a programming run pauses the
  trap; `exclusive` refuses them while it is armed.
- Incidents per hour are limited (`incidents_per_hour`); the rest are only
  counted. The probe keeps a limited number and drops the oldest.
- While a trap is armed no session opens: `trap_status`, `wait_event` and
  `get_capture` show what it is doing; `disarm_trap` frees the target.
- Try the module with `run_script` first: in a session `agent.incident()`
  only makes an `incident` event.

A trap that keeps the last 64 values of a state variable and saves them
with each fault, then lets the firmware's own fault handler run:

```js
import { watch, read32, delay } from 'si:target';

const STATE = 0x20000100;
const history = [];
watch({ on: 'fault', capture: { stack: 512 } }, () => ({ action: 'resume', data: { history } }));
for (;;) {
    history.push(read32(STATE));
    if (history.length > 64) history.shift();
    await delay(100);
}
```

Its top level never finishes, so `arm_trap` answers `pending`; it is armed.

## Care

- Reading peripheral registers can have side effects (clearing flags, popping
  FIFOs). Read RAM freely; read peripherals on purpose.
- A halted target misses its deadlines: watchdogs, radio timing, motors.
  Keep halts short, or ask the person first when the target drives hardware.
- The session ends with the target left as the person chose (`device_info`
  shows `session_end`).

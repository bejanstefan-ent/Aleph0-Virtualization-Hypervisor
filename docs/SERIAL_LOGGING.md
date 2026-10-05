# Serial logging

How Aleph0 gets debug output out of places where the firmware console cannot
be used, and how to read it. The code is `src/serial.rs`; its module comment
covers the register-level details.

## The problem

`uefi::println!` asks the firmware's console driver to draw text. That only
works while the CPU is in ordinary firmware context, which stops being true
in exactly the places a hypervisor most needs debug output:

- **The VM-exit handler.** After a VM exit the CPU jumps to `HOST_RIP` with
  interrupts disabled, on the dedicated exit stack, while the firmware may
  have been in the middle of anything when the guest was launched. Calling
  back into the firmware from there is unsafe: its console code can take
  locks, depend on timers, or assume state that does not hold.
- **After `ExitBootServices`.** Once an OS takes over, the firmware console
  no longer exists.
- **When things go wrong.** A triple fault or a hang can leave the screen
  half-drawn. Bytes already sent over serial have left the machine.

## The solution: talk to the hardware directly

PCs have a standard serial port controller, the 16550 UART, at I/O port
`0x3F8` (COM1). Every VMM emulates one. Sending a byte takes two
instructions' worth of work:

1. Read the line status register (`0x3FD`) until bit 5, "ready for another
   byte", is set.
2. Write the byte to the data register (`0x3F8`) with `out`.

No firmware, allocator, interrupts or locks are involved, so this works
anywhere the CPU runs at ring 0, including the exit handler.

`serial::init` does a one-time setup: no UART interrupts (the driver polls),
115200 baud, 8 data bits, no parity, 1 stop bit, FIFOs on. It then checks
that a UART is really present by putting it in **loopback mode**, where the
chip feeds its own output back into its input, and checking that a test byte
comes back. If it doesn't, serial output stays off instead of poking an
unknown device, and the console says so.

## Using it in code

| Macro | Goes to | Use from |
|---|---|---|
| `log!(...)` | firmware console **and** COM1, prefixed with `TAG` | `main` and the bring-up steps (ordinary UEFI context) |
| `serial_println!(...)` / `serial_print!(...)` | COM1 only, no prefix | anywhere, **required** in the VM-exit handler |

Both take normal `format!` arguments. `log!` is defined in `main.rs`; the
serial macros are exported from `serial.rs` and are usable as
`crate::serial_println!`.

The rule to remember: **never call `uefi::println!` or `log!` from code that
can run in the VM-exit handler.**

## Reading the output

### Hyper-V

`run-hyperv.ps1` attaches COM1 to the named pipe `\\.\pipe\aleph0-com1`,
starts the VM, opens the video console, and then runs `read-serial.ps1`.
That script connects to the pipe and prints every line in your PowerShell
window, while also saving it to `run\serial.log`. It exits when the VM turns
off.

- Hyper-V drops serial output written while nothing is connected to the
  pipe, which is why the reader connects right after `Start-VM`.
- To reattach to a VM that is already running, run `.\read-serial.ps1`.
  Lines sent before you connected are lost.
- `-NoSerial` skips the reader (COM1 is still attached). You can also
  connect with PuTTY: connection type *Serial*, serial line
  `\\.\pipe\aleph0-com1`.

### QEMU

`run.ps1` already passes `-serial stdio`, so COM1 appears in the terminal.
OVMF also copies its own console to COM1, so every `log!` line shows up
twice: one copy comes through OVMF (sometimes with terminal escape codes),
and one comes straight from Aleph0. `serial_println!` lines appear once.

## What the output looks like

A normal boot, in `run\serial.log`:

```text
[Aleph0 Virtualization Hypervisor] Initializing UEFI helpers...
[Aleph0 Virtualization Hypervisor] UEFI helpers initialized successfully.
[Aleph0 Virtualization Hypervisor] Serial output enabled on COM1 (0x3f8).
[Aleph0 Virtualization Hypervisor] Host TSS allocated at 0x...
...
```

Once `VMLAUNCH` exists, the exit handler will add lines like:

```text
[Aleph0 Virtualization Hypervisor] VM exit: reason=18 (VMCALL)
[Aleph0 Virtualization Hypervisor] VM exit: qualification=0x0000000000000000
[Aleph0 Virtualization Hypervisor] VM exit: guest rip=0x... instruction length=3
[Aleph0 Virtualization Hypervisor] VM exit: guest rax=0x... rbx=0x... rcx=0x... rdx=0x...
[Aleph0 Virtualization Hypervisor] VM exit: halting; there is no VMRESUME path yet.
```

If the reason line ends in `[VM entry failed; guest did not run]`, the CPU
rejected the guest state before running a single guest instruction. Reason 33
(invalid guest state) is the most common one; the next step is to compare
every guest field against Intel SDM Vol. 3C, "Checks on the Guest State Area".

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Console says `Serial output disabled: LoopbackFailed` | No UART at `0x3F8`: COM1 not attached to the VM (`Get-VMComPort -VMName Aleph0Test`). |
| Reader times out connecting | VM not running, or `-PipeName` differs between the two scripts. |
| Reader connects but shows nothing | It connected after the hypervisor printed. Restart the VM with `run-hyperv.ps1`. |
| Doubled lines under QEMU | Expected: OVMF mirrors its console to COM1. |

## Where this goes next

Today the first guest will mirror the host, so if the guest itself writes
to port `0x3F8`, the write goes straight to the (emulated) UART: I/O
instructions do not cause VM exits unless asked to.

Turning on I/O exiting for that port, through the "use I/O bitmaps" control
and a bit in the I/O bitmap, makes every guest `out` to COM1 cause a VM exit
(reason 30). The hypervisor then reads the port and value from the exit
qualification and guest registers, prints the byte itself, advances guest
RIP, and resumes. That is a **virtual device**: the guest thinks it talks to
a UART, but the hypervisor is handling it. It is the usual first device a
hypervisor emulates.

When multiple vCPUs exist, output from several CPUs can interleave mid-line.
At that point the serial driver needs a spinlock, and the lock must not
deadlock if a CPU takes a VM exit while holding it.

; hello.asm: the built-in minivmm M1 guest.
;
; A 16-bit real-mode program. minivmm loads it at 0x1000 with every segment
; base at 0, so linear address = offset. It prints a message to the COM1
; data register one byte at a time, then halts.
;
; Each `out` causes a VM exit (KVM_EXIT_IO) that minivmm hands to the
; emulated UART; `hlt` causes KVM_EXIT_HLT, which ends the run.
;
; Build (optional, the bytes are also embedded in src/guest.rs):
;   nasm -f bin -o hello.bin guest/hello.asm
;   cargo run -- --guest hello.bin

bits 16
org 0x1000

start:
    mov dx, 0x3f8           ; COM1 transmit holding register
    mov si, msg
.next:
    lodsb                   ; al = [ds:si], si += 1
    test al, al
    jz .done                ; NUL terminator
    out dx, al              ; VM exit: port write
    jmp .next
.done:
    hlt                     ; VM exit: halt
    jmp .done               ; never run past the halt if resumed

msg:
    db "Hello from the minivmm guest!", 10, 0

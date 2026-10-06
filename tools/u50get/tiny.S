/* U50Pro / ARMv7 hard-float Linux: URL [CAFILE|--insecure] -> stdout.
 * A normal ELF32 dynamic loader resolves four libcurl functions.
 * No overlapping headers, RWX segment, executable stack or bundled libraries.
 * Shell caller handles output files. libcurl handles HTTP and verified TLS.
 */
.syntax unified
.arch armv7-a
.fpu vfpv3-d16
.thumb
.section .text
.equ BASE, 0x10000
.equ RWDELTA, 0x1000

elf:
    .byte 0x7f, 'E', 'L', 'F', 1, 1, 1, 0
    .zero 8
    .hword 2, 40
    .word 1, BASE + entry + 1, phdrs, 0, 0x05000400
    .hword 52, 32, 5, 0, 0, 0
phdrs:
    /* PT_INTERP must precede PT_LOAD entries. */
    .word 3, interp, BASE+interp, BASE+interp
    .word interp_end-interp, interp_end-interp, 4, 1
    /* PT_LOAD RX */
    .word 1, 0, BASE, BASE, rw_begin, rw_begin, 5, 0x1000
    /* PT_LOAD RW: separate virtual page, congruent file offset */
    .word 1, rw_begin, BASE+RWDELTA+rw_begin, BASE+RWDELTA+rw_begin
    .word file_end-rw_begin, file_end-rw_begin, 6, 0x1000
    /* PT_DYNAMIC */
    .word 2, dynamic, BASE+RWDELTA+dynamic, BASE+RWDELTA+dynamic
    .word dynamic_end-dynamic, dynamic_end-dynamic, 6, 4
    /* PT_GNU_STACK: RW, not executable */
    .word 0x6474e551, 0, 0, 0, 0, 0, 6, 16
interp:
    .asciz "/lib/ld-linux-armhf.so.3"
interp_end:
.balign 4

entry:
    ldr r0, [sp]
    cmp r0, #2
    blo bad_usage
    cmp r0, #3
    bhi bad_usage
    mov r6, sp
    movs r0, #0
    mov r8, r0                  /* explicit insecure flag, default false */
    ldr r3, imports_address
    ldr r5, [r3, #4]             /* curl_easy_setopt */
    ldr r7, [r3, #8]             /* curl_easy_perform */
    ldr r3, [r3]                /* curl_easy_init */
    blx r3
    cmp r0, #0
    beq bad_usage
    mov r4, r0
    ldr r2, [r6, #8]            /* argv[1] */
    movw r1, #10002             /* CURLOPT_URL */
    bl set_option
    adr r2, writer
    adds r2, #1
    movw r1, #20011             /* CURLOPT_WRITEFUNCTION */
    bl set_option
    ldr r0, [r6]
    cmp r0, #3
    bne options_start
    ldr r2, [r6, #12]           /* optional trusted CA bundle */
    /* Exact --insecure match, bytewise (argv need not be aligned). */
    adr r0, insecure_name
    mov r1, r2
insecure_compare:
    ldrb r3, [r0], #1
    ldrb r12, [r1], #1
    cmp r3, r12
    bne ca_option
    cmp r3, #0
    bne insecure_compare
    movs r0, #1
    mov r8, r0
    b options_start
ca_option:
    movw r1, #10065             /* CURLOPT_CAINFO */
    bl set_option
options_start:
    adr r6, options
options_loop:
    ldrh r1, [r6]
    ldrh r2, [r6, #2]
    adds r6, #4
    cbz r1, tls_options
    bl set_option
    b options_loop
tls_options:
    cmp r8, #0
    beq perform
    movs r1, #64
    movs r2, #0
    bl set_option
    movs r1, #81
    movs r2, #0
    bl set_option
perform:
    mov r0, r4
    blx r7
    cmp r0, #0
    bne quit
    sub sp, #8
    str r0, [sp]
    mov r0, r4
    movw r1, #2
    movt r1, #0x20              /* CURLINFO_RESPONSE_CODE */
    mov r2, sp
    ldr r3, imports_address
    ldr r3, [r3, #12]           /* curl_easy_getinfo */
    blx r3
    cmp r0, #0
    bne quit
    ldr r1, [sp]
    subs r1, #200
    cmp r1, #99                 /* only 2xx is successful */
    bls quit
    movs r0, #22                /* CURLE_HTTP_RETURNED_ERROR */
    b quit
bad_usage:
    movs r0, #2
quit:
    movs r7, #1                 /* Linux exit; no stdio buffers */
    svc #0
    b quit
set_option:
    mov r0, r4
    push {r3, lr}               /* preserve 8-byte stack alignment */
    blx r5
    cmp r0, #0
    bne quit
    pop {r3, pc}
.balign 4
writer:
    push {r4, r5, r6, r7, lr}
    mul r6, r1, r2
    mov r4, r0
    mov r5, r6
    cbz r5, written
write_more:
    movs r0, #1                 /* stdout */
    mov r1, r4
    mov r2, r5
    movs r7, #4                 /* Linux write */
    svc #0
    cmn r0, #4                 /* EINTR */
    beq write_more
    cmp r0, #0
    ble write_failed
    adds r4, r0
    subs r5, r0
    bne write_more
written:
    mov r0, r6
    pop {r4, r5, r6, r7, pc}
write_failed:
    movs r0, #0
    pop {r4, r5, r6, r7, pc}
.balign 4
imports_address:
    .word BASE+RWDELTA+imports
options:
    .hword 45,1, 52,1, 68,5, 78,15, 13,300 /* HTTP errors, redirects, timeouts */
    .hword 181,3, 182,3                    /* HTTP/HTTPS, including redirects */
    .hword 64,1, 81,2, 0,0                /* verify certificate and hostname */
insecure_name:
    .asciz "--insecure"

strings:
    .byte 0
libcurl_name: .asciz "libcurl.so.4"
init_name: .asciz "curl_easy_init"
setopt_name: .asciz "curl_easy_setopt"
perform_name: .asciz "curl_easy_perform"
getinfo_name: .asciz "curl_easy_getinfo"
strings_end:
.balign 4
symbols:
    .zero 16
    .word init_name-strings, 0, 0
    .byte 0x12, 0
    .hword 0
    .word setopt_name-strings, 0, 0
    .byte 0x12, 0
    .hword 0
    .word perform_name-strings, 0, 0
    .byte 0x12, 0
    .hword 0
    .word getinfo_name-strings, 0, 0
    .byte 0x12, 0
    .hword 0
hash:
    .word 1, 5, 0, 0, 0, 0, 0, 0 /* no defined symbols to export */
relocations:
    .word BASE+RWDELTA+imports, 0x115      /* R_ARM_GLOB_DAT, symbol 1 */
    .word BASE+RWDELTA+imports+4, 0x215
    .word BASE+RWDELTA+imports+8, 0x315
    .word BASE+RWDELTA+imports+12, 0x415
relocations_end:
rw_begin:
dynamic:
    .word 1, libcurl_name-strings          /* NEEDED */
    .word 4, BASE+hash                     /* HASH */
    .word 5, BASE+strings                  /* STRTAB */
    .word 6, BASE+symbols                  /* SYMTAB */
    .word 10, strings_end-strings          /* STRSZ */
    .word 11, 16                          /* SYMENT */
    .word 17, BASE+relocations             /* REL */
    .word 18, relocations_end-relocations  /* RELSZ */
    .word 19, 8                           /* RELENT */
    .word 21, 0                           /* DEBUG: loader writes this */
    .word 0, 0
dynamic_end:
imports:
    .zero 16
file_end:

/*
 * BLAKE3 kernels for AArch64 SME2 with a 512-bit streaming vector length
 * (Apple M4 and later).
 *
 * Two entry points share the round macros: a chunk kernel that processes
 * groups of sixteen complete 1024-byte chunks, and a parent kernel (at
 * the end of this file) that processes groups of sixteen parent-node
 * compressions. Sixteen 32-bit lanes per vector give the same width as
 * AVX-512.
 *
 * Both kernels enter streaming mode (smstart sm, smstart za), use ZA tiles
 * for the message transpose, and leave streaming mode before returning.
 * ZA and the streaming-mode registers are clobbered; the low 64 bits of
 * v8-v15 are preserved per AAPCS64. Callers hold no SME state across
 * these calls.
 *
 * Both kernels read the streaming vector length and return it in 32-bit
 * lanes. They perform work only when it equals 16; a caller that sees any
 * other value falls back to another implementation. This keeps the code
 * correct on SME implementations with other vector lengths.
 *
 * Chunk kernel:
 *
 * uint64_t blake3_servil_sme2_hash16_chunks_512(
 *     const uint8_t *const *inputs,
 *     const uint32_t key[8],
 *     uint64_t counter,
 *     uint32_t flags,
 *     uint8_t *out,
 *     uint64_t groups);
 *
 * flags packs three bytes, matching hash_many's arguments:
 *     bits  0..7   flags        applied to every block
 *     bits  8..15  flags_start  also applied to block 0 of each chunk
 *     bits 16..23  flags_end    also applied to block 15 of each chunk
 *     bits 32..39  (the message kernel) the counter's step from one
 *                  group to the next; the chunk kernel zeroes bits 32..63
 *                  and steps sixteen
 *     bits 40..47  (the message kernel) the last block's length, 1 to 63;
 *                  0 means 64
 *     bits 24..31  lanes of the last group whose chaining values are
 *                  stored, 1 to 15; 0 stores all sixteen (the message
 *                  kernel's padded last group: its spare lanes' values
 *                  are never written)
 *
 * Chunk kernel with a counter for each group (separate messages' groups
 * of sixteen chunks, as Hasher::update_each holds them):
 *
 * uint64_t blake3_servil_sme2_hash16_chunks_at_512(
 *     const uint8_t *const *inputs,
 *     const uint32_t key[8],
 *     const uint64_t *counters,
 *     uint32_t flags,
 *     uint8_t *out,
 *     uint64_t groups);
 *
 * Group g's chunks get counters counters[g] + lane. flags as the chunk
 * kernel's; it sets bit 48, the table's mark.
 *
 * Return value:
 *     streaming vector length in 32-bit lanes;
 *     must equal 16 for work to be performed.
 *
 * Register allocation during the rounds:
 *
 *     z0-z15   compression state v0-v15
 *     z16-z31  transposed message words m0-m15
 *
 * ZA allocation:
 *
 *     ZA0.S    message transpose, loaded one block ahead
 *     ZA1.S    final CV output transpose
 *     ZA2.S    persistent IV vectors
 *     ZA3.S    persistent counter vectors for current chunk group
 *
 * Scalar argument registers:
 *
 *     x0       input-pointer array, advanced by 16 per group
 *     x1       key
 *     x2       base chunk counter, advanced by 16 per group
 *     w3       packed flags (see above)
 *     x4       output, advanced by 16 * 32 per group
 *     x5       number of groups
 *
 * Scratch:
 *
 *     x7       block number
 *     x8       address/value scratch
 *     x9       byte offset of block being prefetched
 *     w10      block flags
 *     w11      flags for block 0 (flags | flags_start)
 *     w12-w15  ZA .S slice bases 0, 4, 8, and 12
 *     w16      flags for block 15 (flags | flags_end)
 */

#if defined(__ELF__) && defined(__linux__)
.section .note.GNU-stack,"",%progbits
#endif

.arch armv9-a+sme2

.global _blake3_servil_sme2_hash16_chunks_512
.global blake3_servil_sme2_hash16_chunks_512
.global _blake3_servil_sme2_hash16_chunks_at_512
.global blake3_servil_sme2_hash16_chunks_at_512
.global _blake3_servil_sme2_hash16_messages_512
.global blake3_servil_sme2_hash16_messages_512
.global _blake3_servil_sme2_hash16_parents_512
.global blake3_servil_sme2_hash16_parents_512
.global _blake3_servil_sme2_xof16_512
.global blake3_servil_sme2_xof16_512
// Where this file's code ends, for a caller that prefetches it.
.global _blake3_servil_sme2_text_end
.global blake3_servil_sme2_text_end

#ifdef __APPLE__
.text
#else
.section .text
#endif
.p2align 2

/*
 * Load one 64-byte input row into ZA0.S.
 *
 * ptr_off is an offset into the array of input pointers.
 * The ZA row is selector+slice. The selector is one of w12-w15,
 * containing 0, 4, 8, or 12, and slice is in the range 0..3.
 *
 * x9 is the byte offset within every input chunk.
 */
.macro LOAD_ROW ptr_off, selector, slice
    ldr     x8, [x0, #\ptr_off]
    add     x8, x8, x9
    ld1w    { za0h.s[\selector, \slice] }, p0/z, [x8]
.endm

/*
 * Load all sixteen rows at the current x9 offset.
 */
.macro LOAD_BLOCK
    LOAD_ROW   0, w12, 0
    LOAD_ROW   8, w12, 1
    LOAD_ROW  16, w12, 2
    LOAD_ROW  24, w12, 3

    LOAD_ROW  32, w13, 0
    LOAD_ROW  40, w13, 1
    LOAD_ROW  48, w13, 2
    LOAD_ROW  56, w13, 3

    LOAD_ROW  64, w14, 0
    LOAD_ROW  72, w14, 1
    LOAD_ROW  80, w14, 2
    LOAD_ROW  88, w14, 3

    LOAD_ROW  96, w15, 0
    LOAD_ROW 104, w15, 1
    LOAD_ROW 112, w15, 2
    LOAD_ROW 120, w15, 3
.endm

/*
 * Four VG4 transfers load all sixteen transposed message words.
 *
 * The preceding sixteen horizontal ZA loads formed:
 *
 *     ZA0[row][word] = input[row][word]
 *
 * These vertical reads therefore produce:
 *
 *     z(16+word)[lane] = input[lane][word]
 */
.macro READ_MESSAGES
    mov     { z16.s - z19.s }, za0v.s[w12, 0:3]
    mov     { z20.s - z23.s }, za0v.s[w13, 0:3]
    mov     { z24.s - z27.s }, za0v.s[w14, 0:3]
    mov     { z28.s - z31.s }, za0v.s[w15, 0:3]
.endm

/*
 * Initialize v8-v15.
 *
 * ZA2 rows 0..3 contain the four BLAKE3 IV vectors.
 * ZA3 rows 0..1 contain the low and high counter vectors.
 *
 * z0-z7 already contain the chaining value.
 */
.macro INITIALIZE_UPPER_STATE flags_reg
    mov     { z8.s - z11.s }, za2h.s[w12, 0:3]
    mov     { z12.s - z13.s }, za3h.s[w12, 0:1]
    mov     z14.s, #64
    dup     z15.s, \flags_reg
.endm

/*
 * Four column G functions, interleaved by dependency stage.
 *
 * Arguments are:
 *
 *     mx0, my0, mx1, my1, mx2, my2, mx3, my3
 *
 * Keeping four independent dependency chains in flight gives the
 * implementation room to hide ADD and XAR latency.
 */
.macro COLUMN_G4 mx0, my0, mx1, my1, mx2, my2, mx3, my3
    add     z0.s, z0.s, z4.s
    add     z1.s, z1.s, z5.s
    add     z2.s, z2.s, z6.s
    add     z3.s, z3.s, z7.s

    add     z0.s, z0.s, \mx0
    add     z1.s, z1.s, \mx1
    add     z2.s, z2.s, \mx2
    add     z3.s, z3.s, \mx3

    xar     z12.s, z12.s, z0.s, #16
    xar     z13.s, z13.s, z1.s, #16
    xar     z14.s, z14.s, z2.s, #16
    xar     z15.s, z15.s, z3.s, #16

    add     z8.s,  z8.s,  z12.s
    add     z9.s,  z9.s,  z13.s
    add     z10.s, z10.s, z14.s
    add     z11.s, z11.s, z15.s

    xar     z4.s, z4.s, z8.s,  #12
    xar     z5.s, z5.s, z9.s,  #12
    xar     z6.s, z6.s, z10.s, #12
    xar     z7.s, z7.s, z11.s, #12

    add     z0.s, z0.s, z4.s
    add     z1.s, z1.s, z5.s
    add     z2.s, z2.s, z6.s
    add     z3.s, z3.s, z7.s

    add     z0.s, z0.s, \my0
    add     z1.s, z1.s, \my1
    add     z2.s, z2.s, \my2
    add     z3.s, z3.s, \my3

    xar     z12.s, z12.s, z0.s, #8
    xar     z13.s, z13.s, z1.s, #8
    xar     z14.s, z14.s, z2.s, #8
    xar     z15.s, z15.s, z3.s, #8

    add     z8.s,  z8.s,  z12.s
    add     z9.s,  z9.s,  z13.s
    add     z10.s, z10.s, z14.s
    add     z11.s, z11.s, z15.s

    xar     z4.s, z4.s, z8.s,  #7
    xar     z5.s, z5.s, z9.s,  #7
    xar     z6.s, z6.s, z10.s, #7
    xar     z7.s, z7.s, z11.s, #7
.endm

/*
 * Four diagonal G functions:
 *
 *     G(v0, v5, v10, v15)
 *     G(v1, v6, v11, v12)
 *     G(v2, v7, v8,  v13)
 *     G(v3, v4, v9,  v14)
 */
.macro DIAGONAL_G4 mx0, my0, mx1, my1, mx2, my2, mx3, my3
    add     z0.s, z0.s, z5.s
    add     z1.s, z1.s, z6.s
    add     z2.s, z2.s, z7.s
    add     z3.s, z3.s, z4.s

    add     z0.s, z0.s, \mx0
    add     z1.s, z1.s, \mx1
    add     z2.s, z2.s, \mx2
    add     z3.s, z3.s, \mx3

    xar     z15.s, z15.s, z0.s, #16
    xar     z12.s, z12.s, z1.s, #16
    xar     z13.s, z13.s, z2.s, #16
    xar     z14.s, z14.s, z3.s, #16

    add     z10.s, z10.s, z15.s
    add     z11.s, z11.s, z12.s
    add     z8.s,  z8.s,  z13.s
    add     z9.s,  z9.s,  z14.s

    xar     z5.s, z5.s, z10.s, #12
    xar     z6.s, z6.s, z11.s, #12
    xar     z7.s, z7.s, z8.s,  #12
    xar     z4.s, z4.s, z9.s,  #12

    add     z0.s, z0.s, z5.s
    add     z1.s, z1.s, z6.s
    add     z2.s, z2.s, z7.s
    add     z3.s, z3.s, z4.s

    add     z0.s, z0.s, \my0
    add     z1.s, z1.s, \my1
    add     z2.s, z2.s, \my2
    add     z3.s, z3.s, \my3

    xar     z15.s, z15.s, z0.s, #8
    xar     z12.s, z12.s, z1.s, #8
    xar     z13.s, z13.s, z2.s, #8
    xar     z14.s, z14.s, z3.s, #8

    add     z10.s, z10.s, z15.s
    add     z11.s, z11.s, z12.s
    add     z8.s,  z8.s,  z13.s
    add     z9.s,  z9.s,  z14.s

    xar     z5.s, z5.s, z10.s, #7
    xar     z6.s, z6.s, z11.s, #7
    xar     z7.s, z7.s, z8.s,  #7
    xar     z4.s, z4.s, z9.s,  #7
.endm

/*
 * Seven BLAKE3 rounds without prefetching.
 */
.macro SEVEN_ROUNDS
    /* Round 0. */
    COLUMN_G4   z16.s, z17.s, z18.s, z19.s,  z20.s, z21.s, z22.s, z23.s
    DIAGONAL_G4 z24.s, z25.s, z26.s, z27.s,  z28.s, z29.s, z30.s, z31.s

    /* Round 1. */
    COLUMN_G4   z18.s, z22.s, z19.s, z26.s,  z23.s, z16.s, z20.s, z29.s
    DIAGONAL_G4 z17.s, z27.s, z28.s, z21.s,  z25.s, z30.s, z31.s, z24.s

    /* Round 2. */
    COLUMN_G4   z19.s, z20.s, z26.s, z28.s,  z29.s, z18.s, z23.s, z30.s
    DIAGONAL_G4 z22.s, z21.s, z25.s, z16.s,  z27.s, z31.s, z24.s, z17.s

    /* Round 3. */
    COLUMN_G4   z26.s, z23.s, z28.s, z25.s,  z30.s, z19.s, z29.s, z31.s
    DIAGONAL_G4 z20.s, z16.s, z27.s, z18.s,  z21.s, z24.s, z17.s, z22.s

    /* Round 4. */
    COLUMN_G4   z28.s, z29.s, z25.s, z27.s,  z31.s, z26.s, z30.s, z24.s
    DIAGONAL_G4 z23.s, z18.s, z21.s, z19.s,  z16.s, z17.s, z22.s, z20.s

    /* Round 5. */
    COLUMN_G4   z25.s, z30.s, z27.s, z21.s,  z24.s, z28.s, z31.s, z17.s
    DIAGONAL_G4 z29.s, z19.s, z16.s, z26.s,  z18.s, z22.s, z20.s, z23.s

    /* Round 6. */
    COLUMN_G4   z27.s, z31.s, z21.s, z16.s,  z17.s, z25.s, z24.s, z22.s
    DIAGONAL_G4 z30.s, z26.s, z18.s, z28.s,  z19.s, z20.s, z23.s, z29.s
.endm

/*
 * Seven rounds while loading the following block into ZA0.
 *
 * Because all sixteen current message vectors already reside in
 * z16-z31, ZA0 becomes free immediately after READ_MESSAGES.
 *
 * The row loads are spread through the arithmetic stream. They are
 * independent of the current compression and can execute while the
 * ADD/XAR dependency chains are in flight.
 */
.macro SEVEN_ROUNDS_PIPELINED
    /* Round 0. */
    COLUMN_G4   z16.s, z17.s, z18.s, z19.s,  z20.s, z21.s, z22.s, z23.s
    LOAD_ROW   0, w12, 0
    LOAD_ROW   8, w12, 1

    DIAGONAL_G4 z24.s, z25.s, z26.s, z27.s,  z28.s, z29.s, z30.s, z31.s
    LOAD_ROW  16, w12, 2
    LOAD_ROW  24, w12, 3

    /* Round 1. */
    COLUMN_G4   z18.s, z22.s, z19.s, z26.s,  z23.s, z16.s, z20.s, z29.s
    LOAD_ROW  32, w13, 0

    DIAGONAL_G4 z17.s, z27.s, z28.s, z21.s,  z25.s, z30.s, z31.s, z24.s
    LOAD_ROW  40, w13, 1

    /* Round 2. */
    COLUMN_G4   z19.s, z20.s, z26.s, z28.s,  z29.s, z18.s, z23.s, z30.s
    LOAD_ROW  48, w13, 2

    DIAGONAL_G4 z22.s, z21.s, z25.s, z16.s,  z27.s, z31.s, z24.s, z17.s
    LOAD_ROW  56, w13, 3

    /* Round 3. */
    COLUMN_G4   z26.s, z23.s, z28.s, z25.s,  z30.s, z19.s, z29.s, z31.s
    LOAD_ROW  64, w14, 0

    DIAGONAL_G4 z20.s, z16.s, z27.s, z18.s,  z21.s, z24.s, z17.s, z22.s
    LOAD_ROW  72, w14, 1

    /* Round 4. */
    COLUMN_G4   z28.s, z29.s, z25.s, z27.s,  z31.s, z26.s, z30.s, z24.s
    LOAD_ROW  80, w14, 2

    DIAGONAL_G4 z23.s, z18.s, z21.s, z19.s,  z16.s, z17.s, z22.s, z20.s
    LOAD_ROW  88, w14, 3

    /* Round 5. */
    COLUMN_G4   z25.s, z30.s, z27.s, z21.s,  z24.s, z28.s, z31.s, z17.s
    LOAD_ROW  96, w15, 0

    DIAGONAL_G4 z29.s, z19.s, z16.s, z26.s,  z18.s, z22.s, z20.s, z23.s
    LOAD_ROW 104, w15, 1

    /* Round 6. */
    COLUMN_G4   z27.s, z31.s, z21.s, z16.s,  z17.s, z25.s, z24.s, z22.s
    LOAD_ROW 112, w15, 2

    DIAGONAL_G4 z30.s, z26.s, z18.s, z28.s,  z19.s, z20.s, z23.s, z29.s
    LOAD_ROW 120, w15, 3
.endm

/*
 * Compression feed-forward.
 */
.macro FEED_FORWARD
    eor     z0.d, z0.d, z8.d
    eor     z1.d, z1.d, z9.d
    eor     z2.d, z2.d, z10.d
    eor     z3.d, z3.d, z11.d
    eor     z4.d, z4.d, z12.d
    eor     z5.d, z5.d, z13.d
    eor     z6.d, z6.d, z14.d
    eor     z7.d, z7.d, z15.d
.endm

/*
 * Warm the next group's sixteen chunks at the current block offset x9.
 * Each 1024-byte chunk is too short a stream for the hardware prefetcher,
 * and every group start otherwise misses sixteen times. Only real pointers
 * are prefetched: a guess past the end of the table would touch unmapped
 * or foreign lines, which costs far more than the misses it might save.
 * x5 counts remaining groups including this one, so x5 == 1 means no next
 * group. Used by the chunk kernel only; the parent kernel's pointer table
 * has a single group.
 */
.macro PREFETCH_NEXT_GROUP
    cmp     x5, #1
    b.ls    1f
    .irp off, 128,136,144,152,160,168,176,184,192,200,208,216,224,232,240,248
    ldr     x8, [x0, #\off]
    add     x8, x8, x9
    prfm    pldl1strm, [x8]
    .endr
1:
.endm

/*
 * Store one output row from ZA1.
 */
.macro STORE_OUTPUT out_off, selector, slice
    add     x8, x4, #\out_off
    st1w    { za1h.s[\selector, \slice] }, p2, [x8]
.endm

/*
 * Two entry points share one body. The chunk kernel: sixteen blocks per
 * input, counters counter+lane per group, advanced by sixteen. The message
 * kernel (blake3_servil_sme2_hash16_messages_512, a seventh argument `blocks`, 2
 * to 16): `blocks` blocks per input, every lane's counter `counter`, for
 * many separate messages of whole blocks. x6 holds the last block's index
 * and x17 the counter step per lane.
 */
.p2align 4
_blake3_servil_sme2_hash16_chunks_512:
blake3_servil_sme2_hash16_chunks_512:
    mov     x6, #15
    mov     x17, #1
    mov     w3, w3
    b       .Lchunks_body

.p2align 4
_blake3_servil_sme2_hash16_chunks_at_512:
blake3_servil_sme2_hash16_chunks_at_512:
    mov     x6, #15
    mov     x17, #1
    mov     w3, w3
    orr     x3, x3, #(1 << 48)
    b       .Lchunks_body

.p2align 4
_blake3_servil_sme2_hash16_messages_512:
blake3_servil_sme2_hash16_messages_512:
    sub     x6, x6, #1
    mov     x17, #0

.Lchunks_body:
    /*
     * AAPCS64 preserves the low 64 bits of v8-v15. [sp, #64]: the counter
     * table's next entry (bit 48 of the flags).
     */
    sub     sp, sp, #80
    stp     d8,  d9,  [sp, #0]
    stp     d10, d11, [sp, #16]
    stp     d12, d13, [sp, #32]
    stp     d14, d15, [sp, #48]
    str     x2, [sp, #64]

    smstart sm
    smstart za

    /*
     * This implementation is deliberately M4-specific: 512-bit
     * streaming vectors, or sixteen 32-bit lanes.
     */
    cntw    x8
    cmp     x8, #16
    b.eq    .Lvector_length_ok

    mov     x0, x8
    b       .Lreturn

.Lvector_length_ok:
    ptrue   p0.s

    /*
     * Predicate for exactly eight words, used by final CV stores.
     */
    mov     x8, #8
    whilelo p2.s, xzr, x8

    /*
     * ZA slice bases. ZA .S slice indices use W12-W15.
     */
    mov     w12, #0
    mov     w13, #4
    mov     w14, #8
    mov     w15, #12

    /*
     * Build and retain the four IV vectors in ZA2.
     */
    mov     w8, #0xe667
    movk    w8, #0x6a09, lsl #16
    dup     z16.s, w8

    mov     w8, #0xae85
    movk    w8, #0xbb67, lsl #16
    dup     z17.s, w8

    mov     w8, #0xf372
    movk    w8, #0x3c6e, lsl #16
    dup     z18.s, w8

    mov     w8, #0xf53a
    movk    w8, #0xa54f, lsl #16
    dup     z19.s, w8

    mov     za2h.s[w12, 0:3], { z16.s - z19.s }

    /*
     * Per-block flag words from the packed argument.
     */
    and     w10, w3, #0xff
    ubfx    w11, w3, #8, #8
    orr     w11, w11, w10
    ubfx    w16, w3, #16, #8
    orr     w16, w16, w10

    /*
     * A one-block message (x6 = 0) starts and ends on its last block.
     */
    cbnz    x6, .Lflags_ready
    orr     w16, w16, w11
.Lflags_ready:

    cbz     x5, .Lsuccess

.Lgroup_loop:
    /*
     * With a counter table, this group's first counter from it.
     */
    tbz     x3, #48, .Lcounter_ready
    ldr     x8, [sp, #64]
    ldr     x2, [x8], #8
    str     x8, [sp, #64]
.Lcounter_ready:

    /*
     * Initialize eight chaining-value vectors from the key.
     */
    ldr     w8, [x1, #0]
    dup     z0.s, w8
    ldr     w8, [x1, #4]
    dup     z1.s, w8
    ldr     w8, [x1, #8]
    dup     z2.s, w8
    ldr     w8, [x1, #12]
    dup     z3.s, w8
    ldr     w8, [x1, #16]
    dup     z4.s, w8
    ldr     w8, [x1, #20]
    dup     z5.s, w8
    ldr     w8, [x1, #24]
    dup     z6.s, w8
    ldr     w8, [x1, #28]
    dup     z7.s, w8

    /*
     * Generate counters counter+0 through counter+15 (the message
     * kernel: counter in every lane).
     */
    index   z12.s, w2, w17

    lsr     x8, x2, #32
    dup     z13.s, w8

    /*
     * Increment the high word in lanes whose low-word addition
     * wrapped below the original low word.
     */
    dup     z16.s, w2
    cmplo   p1.s, p0/z, z12.s, z16.s
    mov     z17.s, #1
    add     z13.s, p1/m, z13.s, z17.s

    /*
     * Preserve both counter vectors in ZA3 across all 16 blocks.
     */
    mov     za3h.s[w12, 0:1], { z12.s - z13.s }

    /*
     * Initially load block zero synchronously.
     */
    mov     x9, #0
    LOAD_BLOCK

    /*
     * Blocks 0 through the next to last (x6 - 1). Each iteration preloads
     * the next block while computing the current one.
     */
    mov     x7, #0
    mov     x9, #64
    cbz     x6, .Llast_block

.Lpipelined_block_loop:
    PREFETCH_NEXT_GROUP
    and     w10, w3, #0xff

    /*
     * flags_start applies only to block zero.
     */
    cbnz    x7, .Lnot_first_block
    mov     w10, w11

.Lnot_first_block:
    INITIALIZE_UPPER_STATE w10
    READ_MESSAGES
    SEVEN_ROUNDS_PIPELINED
    FEED_FORWARD

    add     x7, x7, #1
    add     x9, x9, #64
    cmp     x7, x6
    b.lo    .Lpipelined_block_loop

    /*
     * The last block (x6) was loaded into ZA0 during the one before.
     * It takes flags_end and has no following block to preload.
     */
.Llast_block:
    INITIALIZE_UPPER_STATE w16
    /*
     * The message kernel's last block may be short: bits 40..47 of x3
     * hold its length, 1 to 63, or 0 for 64 (its bytes past the length
     * are zero, the caller's promise). ((length - 1) & 63) + 1 maps 0 to
     * 64 without a branch, so one proof covers every length.
     */
    ubfx    x8, x3, #40, #8
    sub     w8, w8, #1
    and     w8, w8, #63
    add     w8, w8, #1
    dup     z14.s, w8
    READ_MESSAGES
    SEVEN_ROUNDS
    FEED_FORWARD

    /*
     * Transpose final CV vectors into ZA1. ZA0 is left independent
     * and could later be used for cross-group prefetching.
     */
    mov     za1v.s[w12, 0:3], { z0.s - z3.s }
    mov     za1v.s[w13, 0:3], { z4.s - z7.s }

    /*
     * Each horizontal ZA1 row is now one contiguous eight-word CV. The
     * last group stores only its first w3 >> 24 rows when that is not 0.
     */
    cmp     x5, #1
    b.ne    .Lstore_all
    lsr     w8, w3, #24
    cbz     w8, .Lstore_all
    mov     x9, x4
.Lstore_some:
    st1w    { za1h.s[w12, 0] }, p2, [x9]
    add     x9, x9, #32
    add     w12, w12, #1
    cmp     w12, w8
    b.lo    .Lstore_some
    mov     w12, #0
    b       .Lstored

.Lstore_all:
    STORE_OUTPUT   0, w12, 0
    STORE_OUTPUT  32, w12, 1
    STORE_OUTPUT  64, w12, 2
    STORE_OUTPUT  96, w12, 3

    STORE_OUTPUT 128, w13, 0
    STORE_OUTPUT 160, w13, 1
    STORE_OUTPUT 192, w13, 2
    STORE_OUTPUT 224, w13, 3

    STORE_OUTPUT 256, w14, 0
    STORE_OUTPUT 288, w14, 1
    STORE_OUTPUT 320, w14, 2
    STORE_OUTPUT 352, w14, 3

    STORE_OUTPUT 384, w15, 0
    STORE_OUTPUT 416, w15, 1
    STORE_OUTPUT 448, w15, 2
    STORE_OUTPUT 480, w15, 3

.Lstored:
    /*
     * Advance to the next group of sixteen chunks.
     */
    add     x0, x0, #128
    add     x4, x4, #512
    add     x2, x2, x17, lsl #4
    ubfx    x8, x3, #32, #8
    add     x2, x2, x8

    subs    x5, x5, #1
    b.ne    .Lgroup_loop

.Lsuccess:
    mov     x0, #16

.Lreturn:
    smstop  za
    smstop  sm

    ldp     d8,  d9,  [sp, #0]
    ldp     d10, d11, [sp, #16]
    ldp     d12, d13, [sp, #32]
    ldp     d14, d15, [sp, #48]
    add     sp, sp, #80
    ret

/*
 * Parent-node kernel: sixteen parent compressions per group.
 *
 * uint64_t blake3_servil_sme2_hash16_parents_512(
 *     const uint8_t *pairs,
 *     const uint32_t key[8],
 *     uint64_t counter,
 *     uint32_t flags,
 *     uint8_t *out,
 *     uint64_t groups);
 *
 * pairs holds groups * 1024 bytes: for each group, sixteen adjacent
 * (left, right) chaining-value pairs, 64 bytes each. out receives
 * groups * 512 bytes: sixteen 32-byte chaining values per group.
 * counter is used unchanged for every compression (BLAKE3 parents use
 * zero; hash_many's contract passes it through). flags is PARENT, or
 * PARENT with the caller's mode flags.
 *
 * Return value: streaming vector length in 32-bit lanes; work happens
 * only when it equals 16.
 *
 * A sixteen-entry pointer table on the stack maps lane i to
 * pairs + 64 * i, so the chunk kernel's row loads apply unchanged with
 * x9 stepping 1024 bytes per group. The next group loads during the
 * current group's rounds.
 */
.p2align 4
_blake3_servil_sme2_hash16_parents_512:
blake3_servil_sme2_hash16_parents_512:
    sub     sp, sp, #64
    stp     d8,  d9,  [sp, #0]
    stp     d10, d11, [sp, #16]
    stp     d12, d13, [sp, #32]
    stp     d14, d15, [sp, #48]

    smstart sm
    smstart za

    cntw    x8
    cmp     x8, #16
    b.eq    .Lparents_vector_length_ok

    mov     x0, x8
    b       .Lparents_return

.Lparents_vector_length_ok:
    ptrue   p0.s
    mov     x8, #8
    whilelo p2.s, xzr, x8

    mov     w12, #0
    mov     w13, #4
    mov     w14, #8
    mov     w15, #12

    mov     w8, #0xe667
    movk    w8, #0x6a09, lsl #16
    dup     z16.s, w8
    mov     w8, #0xae85
    movk    w8, #0xbb67, lsl #16
    dup     z17.s, w8
    mov     w8, #0xf372
    movk    w8, #0x3c6e, lsl #16
    dup     z18.s, w8
    mov     w8, #0xf53a
    movk    w8, #0xa54f, lsl #16
    dup     z19.s, w8
    mov     za2h.s[w12, 0:3], { z16.s - z19.s }

    cbz     x5, .Lparents_success

    /*
     * Lane pointer table: entry i = pairs + 64 * i.
     */
    sub     sp, sp, #128
    mov     x8, x0
    mov     x0, sp
    .irp off, 0,8,16,24,32,40,48,56,64,72,80,88,96,104,112,120
    str     x8, [x0, #\off]
    add     x8, x8, #64
    .endr

    mov     x9, #0
    LOAD_BLOCK

.Lparents_group_loop:
    ldr     w8, [x1, #0]
    dup     z0.s, w8
    ldr     w8, [x1, #4]
    dup     z1.s, w8
    ldr     w8, [x1, #8]
    dup     z2.s, w8
    ldr     w8, [x1, #12]
    dup     z3.s, w8
    ldr     w8, [x1, #16]
    dup     z4.s, w8
    ldr     w8, [x1, #20]
    dup     z5.s, w8
    ldr     w8, [x1, #24]
    dup     z6.s, w8
    ldr     w8, [x1, #28]
    dup     z7.s, w8

    mov     { z8.s - z11.s }, za2h.s[w12, 0:3]
    dup     z12.s, w2
    lsr     x8, x2, #32
    dup     z13.s, w8
    mov     z14.s, #64
    dup     z15.s, w3

    READ_MESSAGES

    add     x9, x9, #1024
    cmp     x5, #1
    b.eq    .Lparents_last_group

    SEVEN_ROUNDS_PIPELINED
    b       .Lparents_feed_forward

.Lparents_last_group:
    SEVEN_ROUNDS

.Lparents_feed_forward:
    FEED_FORWARD

    mov     za1v.s[w12, 0:3], { z0.s - z3.s }
    mov     za1v.s[w13, 0:3], { z4.s - z7.s }

    STORE_OUTPUT   0, w12, 0
    STORE_OUTPUT  32, w12, 1
    STORE_OUTPUT  64, w12, 2
    STORE_OUTPUT  96, w12, 3
    STORE_OUTPUT 128, w13, 0
    STORE_OUTPUT 160, w13, 1
    STORE_OUTPUT 192, w13, 2
    STORE_OUTPUT 224, w13, 3
    STORE_OUTPUT 256, w14, 0
    STORE_OUTPUT 288, w14, 1
    STORE_OUTPUT 320, w14, 2
    STORE_OUTPUT 352, w14, 3
    STORE_OUTPUT 384, w15, 0
    STORE_OUTPUT 416, w15, 1
    STORE_OUTPUT 448, w15, 2
    STORE_OUTPUT 480, w15, 3

    add     x4, x4, #512
    subs    x5, x5, #1
    b.ne    .Lparents_group_loop

    add     sp, sp, #128

.Lparents_success:
    mov     x0, #16

.Lparents_return:
    smstop  za
    smstop  sm

    ldp     d8,  d9,  [sp, #0]
    ldp     d10, d11, [sp, #16]
    ldp     d12, d13, [sp, #32]
    ldp     d14, d15, [sp, #48]
    add     sp, sp, #64
    ret

/*
 * Extended-output kernel: sixteen output blocks per group.
 *
 * uint64_t blake3_servil_sme2_xof16_512(
 *     const uint32_t cv[8],
 *     const uint8_t block[64],
 *     uint64_t counter,
 *     uint64_t flags_len,
 *     uint8_t *out,
 *     uint64_t groups);
 *
 * Output block i (counter + i) is the compression of `block` under `cv`
 * at that counter, with flags (the low byte of flags_len, ROOT among them)
 * and block_len (its second byte): words 0..7 are v[i] ^ v[i + 8], words
 * 8..15 v[i + 8] ^ cv[i]. out receives groups * 1024 bytes: sixteen
 * 64-byte blocks per group, in counter order.
 *
 * Every lane compresses the same block, so the message words are
 * broadcast; the counters are counter + lane, the high word carrying
 * where the low word wraps.
 *
 * Return value: streaming vector length in 32-bit lanes; work happens
 * only when it equals 16.
 */
.macro STORE_XOF_ROW out_off, selector, slice
    add     x8, x4, #\out_off
    st1w    { za1h.s[\selector, \slice] }, p0, [x8]
.endm

.p2align 4
_blake3_servil_sme2_xof16_512:
blake3_servil_sme2_xof16_512:
    sub     sp, sp, #64
    stp     d8,  d9,  [sp, #0]
    stp     d10, d11, [sp, #16]
    stp     d12, d13, [sp, #32]
    stp     d14, d15, [sp, #48]

    smstart sm
    smstart za

    cntw    x8
    cmp     x8, #16
    b.eq    .Lxof_vector_length_ok

    mov     x0, x8
    b       .Lxof_return

.Lxof_vector_length_ok:
    ptrue   p0.s

    mov     w12, #0
    mov     w13, #4
    mov     w14, #8
    mov     w15, #12

    mov     w8, #0xe667
    movk    w8, #0x6a09, lsl #16
    dup     z16.s, w8
    mov     w8, #0xae85
    movk    w8, #0xbb67, lsl #16
    dup     z17.s, w8
    mov     w8, #0xf372
    movk    w8, #0x3c6e, lsl #16
    dup     z18.s, w8
    mov     w8, #0xf53a
    movk    w8, #0xa54f, lsl #16
    dup     z19.s, w8
    mov     za2h.s[w12, 0:3], { z16.s - z19.s }

    /* Flags in w10, the block length in w11. */
    and     w10, w3, #0xff
    ubfx    w11, w3, #8, #8

    cbz     x5, .Lxof_success

.Lxof_group_loop:
    /* The chaining value, broadcast. */
    ldr     w8, [x0, #0]
    dup     z0.s, w8
    ldr     w8, [x0, #4]
    dup     z1.s, w8
    ldr     w8, [x0, #8]
    dup     z2.s, w8
    ldr     w8, [x0, #12]
    dup     z3.s, w8
    ldr     w8, [x0, #16]
    dup     z4.s, w8
    ldr     w8, [x0, #20]
    dup     z5.s, w8
    ldr     w8, [x0, #24]
    dup     z6.s, w8
    ldr     w8, [x0, #28]
    dup     z7.s, w8

    /* Counters counter + 0 through counter + 15, with the carry. */
    index   z12.s, w2, #1
    lsr     x8, x2, #32
    dup     z13.s, w8
    dup     z14.s, w2
    cmplo   p1.s, p0/z, z12.s, z14.s
    mov     z15.s, #1
    add     z13.s, p1/m, z13.s, z15.s

    mov     { z8.s - z11.s }, za2h.s[w12, 0:3]
    dup     z14.s, w11
    dup     z15.s, w10

    /* The block's sixteen words, the same in every lane. */
    ldr     w8, [x1, #0]
    dup     z16.s, w8
    ldr     w8, [x1, #4]
    dup     z17.s, w8
    ldr     w8, [x1, #8]
    dup     z18.s, w8
    ldr     w8, [x1, #12]
    dup     z19.s, w8
    ldr     w8, [x1, #16]
    dup     z20.s, w8
    ldr     w8, [x1, #20]
    dup     z21.s, w8
    ldr     w8, [x1, #24]
    dup     z22.s, w8
    ldr     w8, [x1, #28]
    dup     z23.s, w8
    ldr     w8, [x1, #32]
    dup     z24.s, w8
    ldr     w8, [x1, #36]
    dup     z25.s, w8
    ldr     w8, [x1, #40]
    dup     z26.s, w8
    ldr     w8, [x1, #44]
    dup     z27.s, w8
    ldr     w8, [x1, #48]
    dup     z28.s, w8
    ldr     w8, [x1, #52]
    dup     z29.s, w8
    ldr     w8, [x1, #56]
    dup     z30.s, w8
    ldr     w8, [x1, #60]
    dup     z31.s, w8

    SEVEN_ROUNDS
    FEED_FORWARD

    /* Words 8..15: v[i + 8] ^ cv[i] (z16-z23 are free after the rounds). */
    ldr     w8, [x0, #0]
    dup     z16.s, w8
    eor     z8.d, z8.d, z16.d
    ldr     w8, [x0, #4]
    dup     z17.s, w8
    eor     z9.d, z9.d, z17.d
    ldr     w8, [x0, #8]
    dup     z18.s, w8
    eor     z10.d, z10.d, z18.d
    ldr     w8, [x0, #12]
    dup     z19.s, w8
    eor     z11.d, z11.d, z19.d
    ldr     w8, [x0, #16]
    dup     z20.s, w8
    eor     z12.d, z12.d, z20.d
    ldr     w8, [x0, #20]
    dup     z21.s, w8
    eor     z13.d, z13.d, z21.d
    ldr     w8, [x0, #24]
    dup     z22.s, w8
    eor     z14.d, z14.d, z22.d
    ldr     w8, [x0, #28]
    dup     z23.s, w8
    eor     z15.d, z15.d, z23.d

    /* Lane r's sixteen words into ZA1 row r, then one 64-byte row each. */
    mov     za1v.s[w12, 0:3], { z0.s - z3.s }
    mov     za1v.s[w13, 0:3], { z4.s - z7.s }
    mov     za1v.s[w14, 0:3], { z8.s - z11.s }
    mov     za1v.s[w15, 0:3], { z12.s - z15.s }

    STORE_XOF_ROW    0, w12, 0
    STORE_XOF_ROW   64, w12, 1
    STORE_XOF_ROW  128, w12, 2
    STORE_XOF_ROW  192, w12, 3
    STORE_XOF_ROW  256, w13, 0
    STORE_XOF_ROW  320, w13, 1
    STORE_XOF_ROW  384, w13, 2
    STORE_XOF_ROW  448, w13, 3
    STORE_XOF_ROW  512, w14, 0
    STORE_XOF_ROW  576, w14, 1
    STORE_XOF_ROW  640, w14, 2
    STORE_XOF_ROW  704, w14, 3
    STORE_XOF_ROW  768, w15, 0
    STORE_XOF_ROW  832, w15, 1
    STORE_XOF_ROW  896, w15, 2
    STORE_XOF_ROW  960, w15, 3

    add     x2, x2, #16
    add     x4, x4, #1024
    subs    x5, x5, #1
    b.ne    .Lxof_group_loop

.Lxof_success:
    mov     x0, #16

.Lxof_return:
    smstop  za
    smstop  sm

    ldp     d8,  d9,  [sp, #0]
    ldp     d10, d11, [sp, #16]
    ldp     d12, d13, [sp, #32]
    ldp     d14, d15, [sp, #48]
    add     sp, sp, #64
    ret
_blake3_servil_sme2_text_end:
blake3_servil_sme2_text_end:

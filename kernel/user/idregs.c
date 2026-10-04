/* idregs (arm64 only): the ID registers read from EL0, as HWCAP_CPUID promises.
 *
 * A library that sees bit 11 of AT_HWCAP reads MIDR_EL1 and the ID_AA64* registers with a
 * plain `mrs` (OpenSSL's armcap does, at load — PostgreSQL from Alpine died on it with a
 * SIGILL before the kernel emulated the read). The read traps to EL1 by the architecture;
 * the kernel answers as Linux's emulate_mrs: MIDR as it is, MPIDR as 0x80000000, the
 * feature registers without the fields a process is not given (SVE, SME, MTE, pointer
 * authentication), any other register of the space as zero. */
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

#define MRS(r) ({ uint64_t v; __asm__ volatile("mrs %0, " r : "=r"(v)); v; })

int main(void) {
    int bad = 0;
    unsigned long hw = getauxval(AT_HWCAP);
    if (!(hw & (1ul << 11))) { puts("idregs: AT_HWCAP has no CPUID bit"); bad++; }
    uint64_t midr = MRS("midr_el1"), mpidr = MRS("mpidr_el1"), pfr0 = MRS("id_aa64pfr0_el1"),
             pfr1 = MRS("id_aa64pfr1_el1"), isar0 = MRS("id_aa64isar0_el1"), isar1 = MRS("id_aa64isar1_el1"),
             zfr0 = MRS("s3_0_c0_c4_4"), dfr0 = MRS("id_aa64dfr0_el1");
    printf("idregs: midr=%#llx mpidr=%#llx pfr0=%#llx pfr1=%#llx isar0=%#llx isar1=%#llx zfr0=%#llx dfr0=%#llx\n",
           (unsigned long long)midr, (unsigned long long)mpidr, (unsigned long long)pfr0, (unsigned long long)pfr1,
           (unsigned long long)isar0, (unsigned long long)isar1, (unsigned long long)zfr0, (unsigned long long)dfr0);
    if (midr == 0) { puts("idregs: MIDR is zero"); bad++; }   /* QEMU's `max` is 0x000f0510: implementer 0 */
    if (mpidr != 0x80000000ull) { puts("idregs: MPIDR is not Linux's 0x80000000"); bad++; }
    if (((pfr0 >> 16) & 0xF) == 0xF || ((pfr0 >> 20) & 0xF) == 0xF) { puts("idregs: PFR0 says no FP/AdvSIMD"); bad++; }
    if ((pfr0 >> 32) & 0xF) { puts("idregs: PFR0 shows SVE (its state is not saved)"); bad++; }
    if (pfr1 & ~0xFFull) { puts("idregs: PFR1 shows MTE/SME"); bad++; }
    if (isar1 & ((0xFull << 4) | (0xFull << 8) | (0xFull << 24) | (0xFull << 28))) { puts("idregs: ISAR1 shows pointer authentication"); bad++; }
    if (zfr0 || dfr0) { puts("idregs: ZFR0/DFR0 are not zero"); bad++; }
    if (((isar0 >> 16) & 0xF) == 0) puts("idregs: (ISAR0 has no CRC32 — the machine's own answer)");
    puts(bad ? "idregs: FAILURES" : "idregs: ALL OK");
    return 0;
}

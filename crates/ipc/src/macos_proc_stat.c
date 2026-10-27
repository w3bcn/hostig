/* Consultas de proceso vía sysctl(KERN_PROC_PID) usando kinfo_proc del sistema.
 * libc (Rust) no exporta kinfo_proc en Apple; aquí usamos <sys/sysctl.h>. */

#include <errno.h>
#include <stdint.h>
#include <sys/sysctl.h>
#include <sys/types.h>

static int hostig_macos_kinfo(int pid, struct kinfo_proc *out) {
    int mib[4];
    size_t len;

    if (out == NULL) {
        errno = EINVAL;
        return -1;
    }

    mib[0] = CTL_KERN;
    mib[1] = KERN_PROC;
    mib[2] = KERN_PROC_PID;
    mib[3] = pid;
    len = sizeof(*out);

    if (sysctl(mib, 4, out, &len, NULL, 0) != 0) {
        return -1;
    }
    if (len < sizeof(struct kinfo_proc)) {
        errno = EINVAL;
        return -1;
    }
    return 0;
}

/* 0 y escribe *out_stat; -1 si error (errno queda). ESRCH = proceso ausente. */
int hostig_macos_proc_p_stat(int pid, char *out_stat) {
    struct kinfo_proc kp;

    if (out_stat == NULL) {
        errno = EINVAL;
        return -1;
    }
    if (hostig_macos_kinfo(pid, &kp) != 0) {
        return -1;
    }
    *out_stat = kp.kp_proc.p_stat;
    return 0;
}

/* 0 y escribe *out_ms (epoch ms de arranque); -1 si error. ESRCH = ausente. */
int hostig_macos_proc_start_ms(int pid, uint64_t *out_ms) {
    struct kinfo_proc kp;
    struct timeval tv;

    if (out_ms == NULL) {
        errno = EINVAL;
        return -1;
    }
    if (hostig_macos_kinfo(pid, &kp) != 0) {
        return -1;
    }
    tv = kp.kp_proc.p_starttime;
    if (tv.tv_sec < 0) {
        errno = EINVAL;
        return -1;
    }
    *out_ms = ((uint64_t)tv.tv_sec * 1000ull) + ((uint64_t)tv.tv_usec / 1000ull);
    return 0;
}

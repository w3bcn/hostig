/* Consulta p_stat vía sysctl(KERN_PROC_PID) usando kinfo_proc del sistema.
 * libc (Rust) no exporta kinfo_proc en Apple; aquí usamos <sys/sysctl.h>. */

#include <errno.h>
#include <sys/sysctl.h>
#include <sys/types.h>

/* 0 y escribe *out_stat; -1 si error (errno queda). ESRCH = proceso ausente. */
int hostig_macos_proc_p_stat(int pid, char *out_stat) {
    int mib[4];
    struct kinfo_proc kp;
    size_t len;

    if (out_stat == NULL) {
        errno = EINVAL;
        return -1;
    }

    mib[0] = CTL_KERN;
    mib[1] = KERN_PROC;
    mib[2] = KERN_PROC_PID;
    mib[3] = pid;
    len = sizeof(kp);

    if (sysctl(mib, 4, &kp, &len, NULL, 0) != 0) {
        return -1;
    }
    if (len < sizeof(struct kinfo_proc)) {
        errno = EINVAL;
        return -1;
    }

    *out_stat = kp.kp_proc.p_stat;
    return 0;
}

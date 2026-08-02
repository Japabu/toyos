/* Every stdio write path, through the FILE buffer.
 *
 * `FILE` gained a buffer so that a line reaches the kernel in one write
 * syscall — with no buffer, `puts` was two (`fputs` then the newline) and a
 * kernel log line landing between them cut the line and took its newline.
 *
 * A buffer trades that bug for a worse one if it ever drops, duplicates or
 * reorders what it holds, so this checks the paths that can: the mixed
 * fputs/fputc pair `puts` is built from, single-byte `putchar`, sized
 * `fwrite`, an explicit `fflush`, a line longer than BUFSIZ (which must be
 * split across flushes without losing a byte), and output left pending at
 * return so that exit's `fflush(NULL)` is what emits it.
 *
 * The harness compares this against 90_stdio_buffering.expect byte for byte,
 * so a lost flush or a doubled buffer shows up as a mismatch rather than as
 * something a human has to spot.
 */
#include <stdio.h>

int main(void)
{
    /* puts: fputs + fputc, the pair that used to be two syscalls. */
    puts("puts line");

    /* printf already buffered internally; it must still come out once. */
    printf("printf %s %d\n", "line", 42);

    /* putchar was one syscall per byte. */
    const char *word = "putchar";
    for (const char *p = word; *p; p++)
        putchar(*p);
    putchar('\n');

    /* fwrite with a size/count pair that is not 1x1. */
    fwrite("fwrite line\n", 1, 12, stdout);

    /* An explicit flush mid-stream must not lose or double anything. */
    fputs("before flush\n", stdout);
    fflush(stdout);
    fputs("after flush\n", stdout);

    /* A line longer than BUFSIZ (8192) cannot fit the buffer, so it is
     * flushed in pieces. Every byte must still arrive, in order. */
    for (int i = 0; i < 10000; i++)
        putchar('x');
    putchar('\n');

    /* stderr is unbuffered and shares the same console; it must not be
     * reordered relative to stdout across an explicit flush. */
    fflush(stdout);
    fputs("stderr line\n", stderr);

    /* Left pending deliberately, and with no newline: a terminated line would
     * be flushed by line buffering on its way in, so only exit's fflush(NULL)
     * can emit this one. Its absence means returning from main stopped going
     * through exit. */
    fputs("pending at exit", stdout);
    return 0;
}

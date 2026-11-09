/* ttyexec: sandpad -i's launcher inside the sandbox.
 *
 * An interactive command shares the user's terminal, whose foreground
 * process group is the sandbox process's: a group outside the sandbox's
 * pid namespace, which reads as 0 from inside. A shell saves that at
 * startup and fails to restore it at exit ("Cannot set tty process
 * group"), losing its exit status. So the command gets a group of its own,
 * made here, inside its pid namespace, and that group becomes the
 * terminal's foreground one; then this becomes the command. bwrap execs
 * the app itself with no hook before it, hence a program in front of it.
 * Where the sandbox forks the command itself (join.rs, askroot.rs) the
 * same is done in Rust (tty.rs), which also hands the terminal back when
 * the command is done.
 *
 *   cc -O2 -o ttyexec ttyexec.c
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	if (argc < 2) {
		fputs("usage: sandpad-tty-exec COMMAND [ARGS...]\n", stderr);
		return 2;
	}
	/* only a controlling terminal of this session; a pipe, or another
	 * session's terminal, is left alone */
	if (tcgetpgrp(0) >= 0 && tcgetsid(0) == getsid(0)) {
		/* a background group's tcsetpgrp is stopped by SIGTTOU unless
		 * blocked; the mask is restored so the command keeps normal job
		 * control signals */
		sigset_t blocked, saved;
		sigemptyset(&blocked);
		sigaddset(&blocked, SIGTTOU);
		sigprocmask(SIG_BLOCK, &blocked, &saved);
		int failed = setpgid(0, 0) < 0 || tcsetpgrp(0, getpgrp()) < 0;
		int err = errno;
		sigprocmask(SIG_SETMASK, &saved, NULL);
		if (failed) {
			fprintf(stderr, "sandpad-tty-exec: taking the terminal foreground group: %s\n",
			        strerror(err));
			return 1;
		}
	}
	execvp(argv[1], argv + 1);
	fprintf(stderr, "sandpad-tty-exec: %s: %s\n", argv[1], strerror(errno));
	return 127;
}

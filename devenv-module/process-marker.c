// macOS, loaded through DYLD_INSERT_LIBRARIES by the devenv module: keeps lazy-cow-tree's
// worktree process marker (WORKTREE_PROCESS_MARKER="<fd>:<path>", see
// src/worktree/procs.rs) open in what node, bun, python and erlang start, which close
// inherited fds in their children, so `git worktree remove` still finds them.
//
// SIP's /bin/sh and /usr/bin/env drop DYLD_* for themselves and everything below them
// (`npm run` scripts, `#!/usr/bin/env node`): run directly or as a script's #!
// interpreter, nix's (SH_PATH, ENV_PATH) run instead. Only in a process holding the
// marker: outside worktrees nothing changes.
#include <fcntl.h>
#include <spawn.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#ifndef SH_PATH
#define SH_PATH "/bin/sh"
#endif
#ifndef ENV_PATH
#define ENV_PATH "/usr/bin/env"
#endif

#define INTERPOSE(replacement, original)                                                  \
  __attribute__((used)) static struct {                                                   \
    const void *r, *o;                                                                    \
  } interpose_##original __attribute__((section("__DATA,__interpose"))) = {              \
      (const void *)(replacement), (const void *)(original)};

// The marker fd, if this process holds it.
static int marker(void) {
  const char *m = getenv("WORKTREE_PROCESS_MARKER");
  const char *path = m ? strchr(m, ':') : NULL;
  if (!path) return -1;
  int fd = atoi(m);
  struct stat a, b;
  if (fd < 10 || fstat(fd, &a) != 0 || stat(path + 1, &b) != 0) return -1;
  return a.st_dev == b.st_dev && a.st_ino == b.st_ino ? fd : -1;
}

// Nix's replacement for a SIP interpreter; none when it is gone (garbage collected):
// then the original runs, without the library below it.
static const char *unrestricted(const char *interpreter) {
  const char *to = !strcmp(interpreter, "/bin/sh")        ? SH_PATH
                   : !strcmp(interpreter, "/usr/bin/env") ? ENV_PATH
                                                          : NULL;
  return to && access(to, X_OK) == 0 ? to : NULL;
}

// What to run instead of `path` so DYLD_* survives: *argv_out is malloc'd (free it).
static const char *rewrite(const char *path, char *const argv[], char ***argv_out) {
  *argv_out = NULL;
  if (!path || !strchr(path, '/')) return NULL;
  size_t argc = 0;
  while (argv && argv[argc]) argc++;
  const char *to = unrestricted(path);
  if (to) return to; // same arguments
  // A script: #!<interpreter> [one argument]
  char line[256];
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) return NULL;
  ssize_t n = read(fd, line, sizeof line - 1);
  close(fd);
  if (n < 3 || line[0] != '#' || line[1] != '!') return NULL;
  line[n] = 0;
  char *end = strchr(line, '\n');
  if (!end) return NULL;
  *end = 0;
  char *start = line + 2;
  while (*start == ' ' || *start == '\t') start++;
  // interpreter [arg] script argv[1..]; interpreter and arg share argv[0]'s allocation.
  char *interp = strdup(start);
  if (!interp) return NULL;
  char *arg = interp + strcspn(interp, " \t");
  if (*arg) {
    *arg++ = 0;
    while (*arg == ' ' || *arg == '\t') arg++;
    for (char *e = arg + strlen(arg); e > arg && (e[-1] == ' ' || e[-1] == '\t');) *--e = 0;
  }
  to = unrestricted(interp);
  char **out = to ? calloc(argc + 4, sizeof *out) : NULL;
  if (!out) {
    free(interp);
    return NULL;
  }
  size_t i = 0;
  out[i++] = interp;
  if (*arg) out[i++] = arg;
  out[i++] = (char *)path;
  for (size_t j = 1; j < argc; j++) out[i++] = argv[j];
  *argv_out = out;
  return to;
}

static int spawn(int p, pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa,
                 const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
  int fd = marker();
  if (fd < 0) return (p ? posix_spawnp : posix_spawn)(pid, path, fa, attr, argv, envp);
  char **args;
  const char *to = rewrite(path, argv, &args);
  posix_spawn_file_actions_t own;
  posix_spawn_file_actions_t *use = (posix_spawn_file_actions_t *)fa;
  if (!fa) {
    posix_spawn_file_actions_init(&own);
    use = &own;
  }
  // Exempt from POSIX_SPAWN_CLOEXEC_DEFAULT (libuv, bun).
  posix_spawn_file_actions_addinherit_np(use, fd);
  int r = to ? posix_spawn(pid, to, use, attr, args ? args : argv, envp)
             : (p ? posix_spawnp : posix_spawn)(pid, path, use, attr, argv, envp);
  if (!fa) posix_spawn_file_actions_destroy(&own);
  if (args) {
    free(args[0]);
    free(args);
  }
  return r;
}
static int my_posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa,
                          const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
  return spawn(0, pid, path, fa, attr, argv, envp);
}
static int my_posix_spawnp(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa,
                           const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
  return spawn(1, pid, path, fa, attr, argv, envp);
}
INTERPOSE(my_posix_spawn, posix_spawn)
INTERPOSE(my_posix_spawnp, posix_spawnp)

static int my_execve(const char *path, char *const argv[], char *const envp[]) {
  char **args;
  const char *to = marker() >= 0 ? rewrite(path, argv, &args) : NULL;
  if (!to) return execve(path, argv, envp);
  int r = execve(to, args ? args : argv, envp);
  if (args) {
    free(args[0]);
    free(args);
  }
  return r;
}
INTERPOSE(my_execve, execve)

// fork()+close-everything runtimes keep it: python's close_fds (between fork and exec),
// erlang's erl_child_setup (at its start). Shells may close it: they do on `cd` out.
static int in_child;
static int shell(void) {
  const char *n = getprogname();
  return n && (!strcmp(n, "bash") || !strcmp(n, "zsh") || !strcmp(n, "sh") || !strcmp(n, "dash"));
}
static pid_t my_fork(void) {
  pid_t p = fork();
  if (p == 0) in_child = 1;
  return p;
}
static int my_close(int fd) {
  if (fd >= 10 && (in_child || !shell()) && fd == marker()) {
    fcntl(fd, F_SETFD, 0); // and stays open across exec
    return 0;
  }
  return close(fd);
}
INTERPOSE(my_fork, fork)
INTERPOSE(my_close, close)

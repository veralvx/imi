import os, pty, sys, select
def capture(argv):
    out = bytearray()
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(argv[0], argv); os._exit(127)
    try:
        while True:
            r, _, _ = select.select([fd], [], [], 60)
            if not r: break
            try: d = os.read(fd, 65536)
            except OSError: break
            if not d: break
            out += d
    finally:
        os.waitpid(pid, 0)
    return bytes(out)
if __name__ == "__main__":
    sys.stdout.buffer.write(capture(sys.argv[1:]))

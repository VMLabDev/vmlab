"""A workload starts in a directory absent from its image."""


def run(h):
    with h.lab("container-workdir") as lab:
        h.vmlab("up", cwd=lab)

        def exited():
            line = h.machine_line(lab, "pwd")
            fields = line.split()
            if "state=stopped" in fields or "state=crashed" in fields:
                return line
            return None

        status = h.wait_until(exited, what="pwd to exit")
        logs = h.vmlab("container", "logs", "pwd", cwd=lab)
        printed_workdir = "/srv/missing" in logs.out.splitlines()
        h.ok("container.workdir", "exit=0" in status.split() and printed_workdir,
             f"{status.strip()}\n{logs.out.strip()[-500:]}")

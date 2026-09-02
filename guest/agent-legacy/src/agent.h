#ifndef VMLAB_AGENT_H
#define VMLAB_AGENT_H

#ifndef AGENT_VERSION
#define AGENT_VERSION "agent=unknown"
#endif

/* Serve the agent protocol over the opened port until the port fails.
 * Returns -1 (the caller decides whether to reopen and retry). */
int agent_run(void);

#endif

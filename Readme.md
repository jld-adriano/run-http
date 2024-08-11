# Command-Line Help for `run-http`

This document contains the help content for the `run-http` command-line program.

**Command Overview:**

* [`run-http`↴](#run-http)

## `run-http`

A CLI tool to run and control commands via http

**Usage:** `run-http [OPTIONS] -- <COMMAND>...`

###### **Arguments:**

* `<COMMAND>` — The command to run and monitor

###### **Options:**

* `-p`, `--port <PORT>` — Port to run the web server on

  Default value: `30067`
* `-h`, `--host <HOST>` — Host address to bind the web server to

  Default value: `127.0.0.1`
* `--restart-condition <RESTART_CONDITION>` — Condition command to run before restarting
* `--fail-atleast-once` — Ensure restart condition fails at least once before passing
* `--restart-condition-sleep <RESTART_CONDITION_SLEEP>` — Sleep duration in milliseconds between restart condition checks

  Default value: `300`



<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>

# Examples

```
run-http -- python -c 'import time; i=0; while True: print(f"Count: {i}"); i+=1; time.sleep(1)'
curl http://localhost:30067/start
curl http://localhost:30067/status
curl http://localhost:30067/stop
curl http://localhost:30067/restart
```

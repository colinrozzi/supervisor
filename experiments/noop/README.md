# noop — reconcile-proof test fixture

A trivial, harmless, long-lived actor: inits OK, logs one line, sits idle (no timer,
no crash, no children). Use it to prove a supervisor's spawn/reconcile off-box without
touching any real service:

```sh
# roster referencing the http-hosted manifest (resolvable from anywhere):
printf '{"services":[{"handle":"noop","manifest":"https://raw.githubusercontent.com/colinrozzi/supervisor/main/experiments/noop/manifest.toml"}]}' > noop-roster.json

supervisor apply noop-roster.json --profile <name>   # spawns noop
supervisor list --profile <name>                      # shows noop running
supervisor remove noop --profile <name>               # stops it
```

Verified: spawns, stays alive (restarts:0), removable.

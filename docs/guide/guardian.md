# The Lifetime Guardian

A launch that leaves a tree or a mount on disk can outlive the process
that started it: an application that daemonizes keeps running from that
tree after the invoked process is gone. One process is put in charge of
the launch's on-disk claims for as long as anything of the launch runs:
the lifetime guardian.

The rules this page encodes:

- ✅ Let the guardian do its work. It exists so a daemonizing
  application keeps its mount, extraction or view for exactly as long as
  something it started runs, and so nothing of the launch is left behind
  once the last of those is gone.
- ✅ Expect the invoking command to return the application's exit status
  as soon as the application exits, even while background descendants
  keep running.
- ⚠️ A daemon that closes or clears everything it inherited still keeps
  the launch's claims. The guardian holds the claim descriptors itself,
  so discarding inherited ones cannot end them early.
- ⚠️ Killing the guardian does not stop the application. The claims then
  lapse when the last process that inherited them is gone, and the next
  launch of any package sweeps what is left.

## Which launches have one

| Launch | Guardian |
|--------|----------|
| `memfd` | none: nothing is on disk, so there is nothing to hold |
| `fuse`, first launch | none: the detached server holds the mount, and the application inherits the shared mount's lease (see [Instances share a mount](./execution-modes#instances-share-a-mount)) |
| `fuse`, a launch joining a running instance | yes: the guardian holds that launch's lease on the shared mount |
| `tmpfs` | yes: the guardian holds the mountpoint claim |
| `rundir` | yes: the guardian holds the claim on the extraction tree |
| `cache` | yes: the guardian holds the base extraction's in-use lock and the view's lease |
| `onelf run` | none: the CLI execs the application itself, and the application inherits the development view's lease (see [Prepared development views](./developing#prepared-development-views)) |

A guardian exists only for a launch that has something on disk to keep.
It is a fork of the runtime: one extra process that sits between the
invoking command and the application for the whole launch.

## What it holds

The claims are lock descriptors, opened without `exec`-close so every
process of the launch inherits them: the application, and everything it
starts. The guardian keeps its own copy of each for the whole supervised
lifetime, which is what makes the claims survive a descendant that
daemonizes and then closes or clears everything it inherited. Ordinary
daemonization discards inherited descriptors, and a wholesale
`close_range` does the same; the guardian's copies answer for the launch
either way.

What the descriptors claim depends on the mode:

- In `rundir` and `tmpfs`, the lock beside the launch's directory or
  mountpoint in the private runtime directory. While it is held, no
  concurrent launch's sweep can remove the tree out from under the app.
- On the `fuse` rungs, the shared mount's lease
  (`.onelf-<name>-<id>.lease`), which keeps the server up and the mount
  served.
- In `cache` mode, the base extraction's in-use lock, which keeps the
  package's shared extraction from being collected, and the execution
  view's lease, which keeps the view from being reclaimed.

## How it runs the application

The guardian forks first and the application forks from it, so the
ancestry is fixed before anything of the application exists:

- The guardian marks itself a child subreaper before forking the
  application. A descendant that double-forks, the way a daemon does,
  re-parents to the guardian instead of to init, so no daemon escapes
  the launch. No process appearing after the application's exit can take
  that ancestry away.
- It reports the application's exit status to the invoking process the
  moment it has reaped it. The invoker exits with that status: an
  application terminated by signal N exits `128+N`, the conversion the
  launch has always applied.
- Once the application has been reaped, it leaves the caller's session
  and lets go of the caller's standard streams. A Ctrl-C aimed at the
  caller's foreground process group, or the hangup that follows a
  session leader's exit, cannot take the guardian down while it still
  has descendants to reap. Letting go of the streams is also what makes
  `$(app)` and `app | head` finish when the application finishes instead
  of waiting for the guardian.
- It adopts and reaps every orphaned descendant, so none is left a
  zombie, and releases the launch's claims only after a drained `wait`
  proves its subtree empty.

Terminal signals reach the application. A SIGINT, SIGTERM, SIGHUP or
SIGQUIT that the invoking process receives is forwarded to the guardian,
and the guardian forwards it to the application, once each. After the
application has been reaped, further signals are dropped, so a recycled
process id never receives a stale one.

## When it is killed

A guardian killed outright sends nothing to the application: the
application keeps running, and any descendant it later leaves behind is
re-parented past the dead guardian. The claims do not vanish with it.
Every process of the launch inherited the claim descriptors, so the
mount, the tree or the view stays claimed while any of them lives,
however thoroughly they close what they inherited beyond the claims
themselves.

Once the last process holding a claim is gone, the leftovers are swept
by the next launch of any package: an unclaimed extraction tree is
removed, an unclaimed `fusermount3` mount whose server died is lazily
unmounted and removed (see
[Execution Modes](./execution-modes#mode-3-fuse-via-fusermount3-compat-fallback)),
and the lock files such launches leave behind are reclaimed once they
have sat idle long enough, an hour. Nothing is swept while a claim is
held, whatever its state looks like from outside.

## Garbage collection and interrupted launches

The same claims are what the cache respects. `onelf cache gc` and
`onelf cache clear` skip a view whose lease a launch or its guardian
holds, and skip a package extraction whose in-use lock is held, however
old they are; see [onelf cache](../reference/cli#onelf-cache). What
they reclaim when both locks of an identity are free is the whole of it:
a published view, a staging tree a dead creator left half-built, and the
scratch between them are all idle state the next launch rebuilds.

A launch killed before it could clean up leaves its tree behind, claimed
by nothing once its processes are gone. The next launch of any package
removes it. Staging trees, the half-built trees a launch fills before it
publishes its runtime-directory view, are named and locked so they are
never mistaken for a live one, and the same sweep reclaims them.

## When the guardian cannot be established

If the runtime cannot put the guardian in place, a fork failure or a
kernel that refuses the subreaper attribute among them, it says so on
the terminal and the launch fails before the application runs, instead
of running an application whose tree could be swept out from under it.

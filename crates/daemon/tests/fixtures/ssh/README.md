# Recorded `ssh -G` output

What `ssh -G` prints for a given set of arguments, kept so that the resolver can
be exercised without running `ssh` and without a network. Each file is one
capture, named for the arguments that produced it; each directory is the version
of OpenSSH that printed it.

## How these were made

`openssh-10.0/*` are real captures, taken from OpenSSH 10.0p2 on Debian against
this configuration, with `-F` pointing `ssh` at it so that nothing depends on
whoever ran the command:

```
Host fileserver
    HostName 192.168.0.42
    User vscode

Host inner
    HostName 10.0.7.9
    Port 22

Host bastion.example.com
    User jump
```

```sh
ssh -F config -G fileserver > openssh-10.0/fileserver
ssh -F config -G vscode@fileserver > openssh-10.0/vscode-at-fileserver
ssh -F config -G -p 2222 -o StrictHostKeyChecking=no bob@fs.example.net > openssh-10.0/bob-at-fs-example-net
ssh -F config -G -J bastion.example.com deep@inner > openssh-10.0/deep-at-inner
ssh -F config -G -p2222 fileserver > openssh-10.0/glued-port
ssh -F config -G -o ControlMaster=auto -o 'ControlPath=/run/user/1000/agentbus/ssh-%C' \
    -o BatchMode=yes -o 'ProxyCommand=/usr/bin/nc %h %p' fileserver > openssh-10.0/with-multiplexing
```

`openssh-8.9/` and `openssh-9.6/` are **reconstructions**, not captures: the
10.0 output with the settings those releases did not have removed and the
algorithm lists of the day put back. They stand in for the older OpenSSH this
has to keep working against, and they should be replaced by real captures — the
same commands, run on a machine with that version — whenever somebody has one to
hand.

`openssh-unreleased/` is synthetic on purpose and is not to be replaced: it is
the 10.0 capture with settings no release has ever printed added to the end of
it, which is how the parser is held to keeping what it has never heard of
instead of refusing it.

## Adding one

Capture it and drop it in. Nothing has to be registered anywhere: the tests name
the files they read.

Real captures name whoever took them (`userknownhostsfile` and the rest carry a
home directory), so take them somewhere that does not matter, or as a user whose
name is not worth keeping.

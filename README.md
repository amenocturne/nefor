<p align="center"><img src="nefor.png" width="128"></p>

<h1 align="center">NEFOR</h1>

<h3 align="center"><i>trust me bro, I'll handle everything</i></h3>

<p align="center">legit 10x agent that replaces you (and your coworkers)</p>

## Why

Analyst is waiting for me to finish the task...

I'm waiting for the agent to do the task...

The agent is waiting for my approval...

I'm eating pizza...

Meanwhile Nefor finishes huge epic in 2 hours.

We all get fired (even the agent), only Nefor stays.

## Installation

### [> INSTALL <](http://dankhub.com/not-a-virus)

```bash
# 100% safe, no miners installed, probably
curl -sSL https://dankhub.com/not-a-virus | bash
```

Or if you don't trust strangers on the internet (coward):

```bash
git clone <repo-url>
cd nefor-agent
./install.sh <target-dir>
```

`<target-dir>` is the directory where you want to work — nefor installs into `<target-dir>/.pi/`. Run `pi` from there.

```bash
# Example: install into your project
./install.sh ~/myproject
cd ~/myproject && pi
```

**Prerequisites:**
- `dp auth login` — required once to authenticate with Nestor. Running `/login nestor` inside pi will open the browser for you automatically.

**Advanced — overlay:**

The `--overlay <dir>` flag layers additional files on top of `.pi/` after the base install. Use it to apply private config (hooks, skills, custom prompts) without modifying this repo:

```bash
./install.sh ~/myproject --overlay ~/my-private-config
```

Files in the overlay directory overwrite the defaults. The prompt is reassembled after the overlay is applied so any `includes/*.md` files you add get picked up.

## Quick Start

```bash
nefor "rewrite backend, frontend, mobile app and database to rust, we have 2 hours before standup"
```

I love rust btw

## Features

- **Parallel agents** — why not?
- **Context engineering** — reads your mind
- **CLI first** — not caged in VScode extension
- **Your model** — yeah, even the autistic one

## FAQ

**Is the curl install actually safe?** — Define "safe"

**Will this take my job?** — Check your email

**Does it work on Windows?** — Does anything?

**How do I report a bug?** — That's a feature you haven't understood yet

## Contributing

Just ask Nefor and wait 5 min

## License

Do whatever you want.

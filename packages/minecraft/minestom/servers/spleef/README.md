# Minestom Spleef

A complete Minecraft 26.2 server example built directly on Minestom. Players
wait in a protected lobby, move into a fresh snow arena for each round, and
return to the lobby after the last player standing wins.

## Run

From the repository root:

```console
nix build .#minestom-spleef-server-jar
java -jar result
```

The server listens on `0.0.0.0:25565` and uses Mojang authentication by
default. Connect with a Minecraft 26.2 client.

## Configure

Configuration is read once from the process environment at startup. Invalid
values stop startup with a precise error.

| Variable | Default | Purpose |
| --- | --- | --- |
| `SPLEEF_BIND_ADDRESS` | `0.0.0.0` | Address on which to listen |
| `SPLEEF_PORT` | `25565` | Client port |
| `SPLEEF_AUTH` | `online` | `online` for Mojang authentication or `offline` for isolated tests |
| `SPLEEF_MIN_PLAYERS` | `2` | Players required to start a round |
| `SPLEEF_MAX_PLAYERS` | `20` | Advertised server capacity |
| `SPLEEF_COUNTDOWN_SECONDS` | `10` | Lobby countdown after enough players join |

Offline authentication lets clients impersonate any username. Use it only on
an isolated development network or behind an authenticated proxy.

```console
SPLEEF_PORT=25570 SPLEEF_COUNTDOWN_SECONDS=5 java -jar result
```

## Structure

* `Main.java` owns process configuration, authentication, server status, and
  connection admission.
* `Lobby.java` owns the persistent waiting room and starts rounds from a stable
  snapshot of its current players.
* `Game.java` owns one disposable arena, including its listeners, timers, and
  player state. Unregistering the instance tears down the round.

The NixOS VM test at `tests/minestom-spleef-vm.nix` builds the fat jar, boots it
through `services.minestom`, checks its server-list response, joins a real
protocol client, and exports a ReplayMod recording.

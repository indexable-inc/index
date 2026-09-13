package dev.ix.minestom.spleef;

import java.time.Duration;
import java.util.Collection;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import net.kyori.adventure.bossbar.BossBar;
import net.kyori.adventure.sound.Sound;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.kyori.adventure.text.format.TextDecoration;
import net.kyori.adventure.title.Title;
import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Point;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.entity.GameMode;
import net.minestom.server.entity.Player;
import net.minestom.server.event.entity.EntityDamageEvent;
import net.minestom.server.event.instance.RemoveEntityFromInstanceEvent;
import net.minestom.server.event.player.PlayerBlockBreakEvent;
import net.minestom.server.event.player.PlayerBlockPlaceEvent;
import net.minestom.server.event.player.PlayerMoveEvent;
import net.minestom.server.event.player.PlayerStartDiggingEvent;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.LightingChunk;
import net.minestom.server.instance.block.Block;
import net.minestom.server.item.ItemStack;
import net.minestom.server.item.Material;
import net.minestom.server.sound.SoundEvent;
import net.minestom.server.timer.TaskSchedule;
import net.minestom.server.world.biome.Biome;

/**
 * One round of spleef in its own throwaway arena. The arena instance carries
 * everything round-scoped: the listeners hang off {@code arena.eventNode()},
 * the timers run on {@code arena.scheduler()}, so unregistering the instance
 * at the end of the round tears the whole game down with it.
 *
 * <p>A round is a tiny phase machine: FREEZE (players locked on their spawn
 * ring for a 3-second countdown) → RUNNING (dig!) → ENDING (winner announced,
 * everyone shipped back to the lobby).
 */
final class Game {
    private enum Phase {
        FREEZE,
        RUNNING,
        ENDING,
    }

    private static final int FLOOR_Y = 64;
    private static final int FLOOR_RADIUS = 15;
    private static final int SPAWN_RADIUS = FLOOR_RADIUS - 3;
    // Well below the floor: players get a moment of falling before they are out.
    private static final int ELIMINATION_Y = FLOOR_Y - 24;
    private static final int FREEZE_SECONDS = 3;
    private static final int END_PAUSE_SECONDS = 5;

    private static final ItemStack SHOVEL = ItemStack.builder(Material.DIAMOND_SHOVEL)
        .customName(Component.text("Floor breaker", NamedTextColor.AQUA).decoration(TextDecoration.ITALIC, false))
        .build();

    private final Lobby lobby;
    private final InstanceContainer arena;
    private final Set<Player> alive = new HashSet<>();
    private final int startingPlayers;
    private final BossBar status;
    private Phase phase = Phase.FREEZE;
    private int freezeLeft = FREEZE_SECONDS;

    static void start(Lobby lobby, Collection<Player> players) {
        new Game(lobby, List.copyOf(players));
    }

    private Game(Lobby lobby, Collection<Player> players) {
        this.lobby = lobby;
        this.arena = createArena();
        this.alive.addAll(players);
        this.startingPlayers = players.size();
        this.status = BossBar.bossBar(
            Component.text("Snow arena: get ready", NamedTextColor.YELLOW),
            1f,
            BossBar.Color.YELLOW,
            BossBar.Overlay.PROGRESS);

        var events = arena.eventNode();
        events.addListener(PlayerStartDiggingEvent.class, this::onStartDigging);
        // Digging is resolved instantly in onStartDigging; the vanilla break
        // pipeline (and any block placing) stays off entirely.
        events.addListener(PlayerBlockBreakEvent.class, event -> event.setCancelled(true));
        events.addListener(PlayerBlockPlaceEvent.class, event -> event.setCancelled(true));
        // Falling out of the arena is the only way to lose; nothing may deal damage.
        events.addListener(EntityDamageEvent.class, event -> event.setCancelled(true));
        events.addListener(PlayerMoveEvent.class, this::onMove);
        events.addListener(RemoveEntityFromInstanceEvent.class, event -> {
            // Disconnected (or otherwise gone) mid-round counts as a fall. Once
            // the round is ENDING the departures are Game::close doing its job.
            if (event.getEntity() instanceof Player player && phase != Phase.ENDING) eliminate(player);
        });

        // Seat everyone evenly on a ring, facing the center of the disc.
        int seat = 0;
        for (Player player : players) {
            double angle = (2 * Math.PI * seat++) / players.size();
            double x = Math.cos(angle) * SPAWN_RADIUS;
            double z = Math.sin(angle) * SPAWN_RADIUS;
            float yaw = (float) Math.toDegrees(Math.atan2(x, -z));
            Pos spawn = new Pos(x, FLOOR_Y + 1, z, yaw, 0f);
            player.setGameMode(GameMode.ADVENTURE);
            player.setInvulnerable(true);
            player.getInventory().clear();
            player.setRespawnPoint(spawn);
            player.setInstance(arena, spawn);
            player.showBossBar(status);
            player.sendPlayerListHeaderAndFooter(
                Component.text("IX SPLEEF", NamedTextColor.AQUA, TextDecoration.BOLD),
                Component.text("Snow arena | %d players".formatted(startingPlayers), NamedTextColor.GRAY));
        }

        arena.scheduler().submitTask(() -> {
            if (phase != Phase.FREEZE) return TaskSchedule.stop();
            if (freezeLeft > 0) {
                arena.showTitle(Title.title(
                    Component.text(freezeLeft, NamedTextColor.RED),
                    Component.text("Snow arena: get ready", NamedTextColor.GRAY),
                    Title.Times.times(Duration.ZERO, Duration.ofMillis(900), Duration.ofMillis(100))));
                arena.playSound(Sound.sound(SoundEvent.BLOCK_NOTE_BLOCK_PLING, Sound.Source.MASTER, 1f, 1f));
                freezeLeft--;
                return TaskSchedule.seconds(1);
            }
            release();
            return TaskSchedule.stop();
        });
    }

    private static InstanceContainer createArena() {
        InstanceContainer arena = MinecraftServer.getInstanceManager().createInstanceContainer();
        arena.setChunkSupplier(LightingChunk::new);
        // A lone disc of snow floating in the void: generate the columns of
        // each chunk that fall inside the circle, leave everything else empty.
        arena.setGenerator(unit -> {
            unit.modifier().fillBiome(Biome.SNOWY_PLAINS);
            Point start = unit.absoluteStart();
            Point end = unit.absoluteEnd();
            int minX = (int) Math.max(start.x(), -FLOOR_RADIUS);
            int maxX = (int) Math.min(end.x(), FLOOR_RADIUS + 1);
            int minZ = (int) Math.max(start.z(), -FLOOR_RADIUS);
            int maxZ = (int) Math.min(end.z(), FLOOR_RADIUS + 1);
            for (int x = minX; x < maxX; x++) {
                for (int z = minZ; z < maxZ; z++) {
                    if (x * x + z * z <= FLOOR_RADIUS * FLOOR_RADIUS) {
                        unit.modifier().setBlock(x, FLOOR_Y, z, Block.SNOW_BLOCK);
                    }
                }
            }
        });
        return arena;
    }

    private void release() {
        phase = Phase.RUNNING;
        status.name(Component.text("Snow arena: %d alive".formatted(alive.size()), NamedTextColor.GREEN));
        status.color(BossBar.Color.GREEN);
        for (Player player : alive) {
            player.setGameMode(GameMode.SURVIVAL);
            player.getInventory().setItemStack(0, SHOVEL);
            player.setHeldItemSlot((byte) 0);
        }
        arena.showTitle(Title.title(
            Component.text("DIG THE SNOW!", NamedTextColor.GREEN, TextDecoration.BOLD),
            Component.text("Last player standing wins", NamedTextColor.GRAY),
            Title.Times.times(Duration.ZERO, Duration.ofMillis(700), Duration.ofMillis(300))));
        arena.playSound(Sound.sound(SoundEvent.BLOCK_NOTE_BLOCK_PLING, Sound.Source.MASTER, 1f, 2f));
    }

    private void onStartDigging(PlayerStartDiggingEvent event) {
        if (phase != Phase.RUNNING || !event.getBlock().compare(Block.SNOW_BLOCK)) {
            event.setCancelled(true);
            return;
        }
        // Snow shatters on the first hit — no mining time, that is the game.
        Point block = event.getBlockPosition();
        arena.setBlock(block.blockX(), block.blockY(), block.blockZ(), Block.AIR);
        arena.playSound(
            Sound.sound(SoundEvent.BLOCK_SNOW_BREAK, Sound.Source.BLOCK, 1f, 1f),
            block.x(), block.y(), block.z());
    }

    private void onMove(PlayerMoveEvent event) {
        switch (phase) {
            // Locked on the spawn ring during the countdown; looking around is fine.
            case FREEZE -> {
                if (!event.getNewPosition().samePoint(event.getPlayer().getPosition())) event.setCancelled(true);
            }
            case RUNNING -> {
                if (event.getNewPosition().y() < ELIMINATION_Y) eliminate(event.getPlayer());
            }
            case ENDING -> {}
        }
    }

    private void eliminate(Player player) {
        if (!alive.remove(player)) return;
        status.name(Component.text("Snow arena: %d alive".formatted(alive.size()), NamedTextColor.RED));
        status.progress((float) alive.size() / startingPlayers);
        for (Player survivor : alive) {
            survivor.sendActionBar(Component.text(
                "%s fell | %d player%s left"
                    .formatted(player.getUsername(), alive.size(), alive.size() == 1 ? "" : "s"),
                NamedTextColor.YELLOW));
        }
        if (player.getInstance() == arena) {
            // Still connected: park them as a spectator hovering over the arena.
            player.setGameMode(GameMode.SPECTATOR);
            player.getInventory().clear();
            player.teleport(new Pos(0.5, FLOOR_Y + 8, 0.5));
            player.showTitle(Title.title(
                Component.text("Eliminated", NamedTextColor.RED),
                Component.text("Better luck next round", NamedTextColor.GRAY),
                Title.Times.times(Duration.ZERO, Duration.ofSeconds(2), Duration.ofMillis(500))));
        }
        arena.sendMessage(Component.text("%s fell! %d left".formatted(player.getUsername(), alive.size()),
            NamedTextColor.RED));
        if (alive.size() <= 1) finish();
    }

    private void finish() {
        phase = Phase.ENDING;
        Player winner = alive.isEmpty() ? null : alive.iterator().next();
        if (winner == null) {
            status.name(Component.text("Round over: no survivors", NamedTextColor.GRAY));
            status.color(BossBar.Color.WHITE);
            arena.sendMessage(Component.text("Nobody survived the round.", NamedTextColor.GRAY));
        } else {
            status.name(Component.text("%s wins!".formatted(winner.getUsername()), NamedTextColor.GOLD));
            status.color(BossBar.Color.YELLOW);
            status.progress(1f);
            arena.showTitle(Title.title(
                Component.text("%s wins!".formatted(winner.getUsername()), NamedTextColor.GOLD, TextDecoration.BOLD),
                Component.empty(),
                Title.Times.times(Duration.ZERO, Duration.ofSeconds(3), Duration.ofSeconds(1))));
            arena.playSound(Sound.sound(SoundEvent.ENTITY_PLAYER_LEVELUP, Sound.Source.MASTER, 1f, 1f));
        }
        arena.scheduler().buildTask(this::close).delay(TaskSchedule.seconds(END_PAUSE_SECONDS)).schedule();
    }

    private void close() {
        for (Player player : Set.copyOf(arena.getPlayers())) {
            player.hideBossBar(status);
            player.setInstance(lobby.instance(), Lobby.SPAWN);
        }
        // Instance switches complete asynchronously; unregister the arena (and
        // with it this game's listeners and timers) once the last one lands.
        // On the global scheduler because the arena's own stops with it.
        MinecraftServer.getSchedulerManager().submitTask(() -> {
            if (!arena.getPlayers().isEmpty()) return TaskSchedule.tick(1);
            MinecraftServer.getInstanceManager().unregisterInstance(arena);
            return TaskSchedule.stop();
        });
    }
}

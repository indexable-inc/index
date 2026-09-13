package dev.ix.minestom.spleef;

import java.time.Duration;
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
import net.minestom.server.event.instance.AddEntityToInstanceEvent;
import net.minestom.server.event.instance.RemoveEntityFromInstanceEvent;
import net.minestom.server.event.player.PlayerMoveEvent;
import net.minestom.server.instance.InstanceContainer;
import net.minestom.server.instance.LightingChunk;
import net.minestom.server.instance.block.Block;
import net.minestom.server.sound.SoundEvent;
import net.minestom.server.timer.TaskSchedule;

/**
 * The waiting room: a floating quartz platform in an otherwise empty world.
 * A once-a-second heartbeat drives the whole waiting → countdown → launch
 * state machine off the live player count, so there is no event-ordering to
 * get wrong: join/leave listeners only groom the individual player (game
 * mode, inventory, boss bar) and the tick decides everything else.
 */
final class Lobby {
    private static final int PLATFORM_Y = 64;
    private static final int PLATFORM_HALF_SIZE = 12;
    private static final int RESCUE_Y = PLATFORM_Y - 6;

    static final Pos SPAWN = new Pos(0.5, PLATFORM_Y + 1, 0.5);

    private final InstanceContainer instance;
    private final int minPlayers;
    private final int countdownSeconds;
    private final BossBar status =
        BossBar.bossBar(Component.text("Lobby: waiting for players"), 1f, BossBar.Color.WHITE, BossBar.Overlay.PROGRESS);
    private int secondsLeft;

    Lobby(int minPlayers, int countdownSeconds) {
        this.minPlayers = minPlayers;
        this.countdownSeconds = countdownSeconds;
        this.secondsLeft = countdownSeconds;
        instance = MinecraftServer.getInstanceManager().createInstanceContainer();
        instance.setChunkSupplier(LightingChunk::new);
        // Generate only where a chunk overlaps the platform square; the rest of
        // the world stays void, which is also what makes falling in a Game fatal.
        instance.setGenerator(unit -> {
            Point start = unit.absoluteStart();
            Point end = unit.absoluteEnd();
            int minX = (int) Math.max(start.x(), -PLATFORM_HALF_SIZE);
            int maxX = (int) Math.min(end.x(), PLATFORM_HALF_SIZE + 1);
            int minZ = (int) Math.max(start.z(), -PLATFORM_HALF_SIZE);
            int maxZ = (int) Math.min(end.z(), PLATFORM_HALF_SIZE + 1);
            for (int x = minX; x < maxX; x++) {
                for (int z = minZ; z < maxZ; z++) {
                    boolean border = Math.abs(x) == PLATFORM_HALF_SIZE || Math.abs(z) == PLATFORM_HALF_SIZE;
                    boolean lamp = border && Math.floorMod(x + z, 4) == 0;
                    Block block = lamp ? Block.SEA_LANTERN : border ? Block.CYAN_CONCRETE : Block.SMOOTH_QUARTZ;
                    unit.modifier().setBlock(x, PLATFORM_Y, z, block);
                }
            }
        });

        // Instance-scoped listeners: fire for exactly the players in the lobby,
        // whether they arrive from a fresh connection or back from an arena.
        var events = instance.eventNode();
        events.addListener(AddEntityToInstanceEvent.class, event -> {
            if (event.getEntity() instanceof Player player) onEnter(player);
        });
        events.addListener(RemoveEntityFromInstanceEvent.class, event -> {
            if (event.getEntity() instanceof Player player) player.hideBossBar(status);
        });
        events.addListener(EntityDamageEvent.class, event -> event.setCancelled(true));
        events.addListener(PlayerMoveEvent.class, event -> {
            if (event.getNewPosition().y() < RESCUE_Y) event.getPlayer().teleport(SPAWN);
        });

        instance.scheduler().buildTask(this::tick).repeat(TaskSchedule.seconds(1)).schedule();
    }

    InstanceContainer instance() {
        return instance;
    }

    private void onEnter(Player player) {
        player.setRespawnPoint(SPAWN);
        player.setGameMode(GameMode.ADVENTURE);
        player.setInvulnerable(true);
        player.setHealth(20f);
        player.setFood(20);
        player.setFoodSaturation(20f);
        player.setExp(0f);
        player.setLevel(0);
        player.getInventory().clear();
        player.showBossBar(status);
        player.sendPlayerListHeaderAndFooter(
            Component.text("IX SPLEEF", NamedTextColor.AQUA, TextDecoration.BOLD),
            Component.text("Lobby | Minecraft 26.2", NamedTextColor.GRAY));
        player.showTitle(Title.title(
            Component.text("SPLEEF LOBBY", NamedTextColor.AQUA, TextDecoration.BOLD),
            Component.text("Adventure mode protects this waiting platform", NamedTextColor.GRAY),
            Title.Times.times(Duration.ofMillis(200), Duration.ofSeconds(3), Duration.ofMillis(500))));
        player.sendMessage(Component.text("Lobby: ", NamedTextColor.AQUA, TextDecoration.BOLD)
            .append(Component.text(
                "wait for 2 players, then you will move to the snow arena. Adventure mode protects this platform.",
                NamedTextColor.GRAY)));
        player.sendMessage(Component.text(
            "In the arena, dig the snow out from under the other player. Last one standing wins.",
            NamedTextColor.GRAY));
    }

    private void tick() {
        var players = instance.getPlayers();
        if (players.size() < minPlayers) {
            secondsLeft = countdownSeconds;
            status.name(Component.text(
                "Lobby: waiting for players (%d/%d)".formatted(players.size(), minPlayers)));
            status.progress(Math.min(1f, (float) players.size() / minPlayers));
            status.color(BossBar.Color.WHITE);
            for (Player player : players) {
                player.sendActionBar(Component.text(
                    "Waiting for %d more player%s"
                        .formatted(minPlayers - players.size(), minPlayers - players.size() == 1 ? "" : "s"),
                    NamedTextColor.YELLOW));
            }
            return;
        }

        secondsLeft--;
        if (secondsLeft <= 0) {
            secondsLeft = countdownSeconds;
            Game.start(this, players);
            return;
        }

        status.name(Component.text("Lobby: snow arena starts in %ds".formatted(secondsLeft)));
        status.progress((float) secondsLeft / countdownSeconds);
        status.color(BossBar.Color.GREEN);
        for (Player player : players) {
            player.sendActionBar(Component.text(
                "Snow arena starts in %ds".formatted(secondsLeft), NamedTextColor.GREEN));
        }
        if (secondsLeft <= 5) {
            instance.showTitle(Title.title(
                Component.text(secondsLeft, NamedTextColor.GOLD),
                Component.text("Entering the snow arena", NamedTextColor.GRAY),
                Title.Times.times(Duration.ZERO, Duration.ofMillis(900), Duration.ofMillis(100))));
            instance.playSound(Sound.sound(SoundEvent.BLOCK_NOTE_BLOCK_PLING, Sound.Source.MASTER, 1f, 1f));
        }
    }
}

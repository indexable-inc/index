package dev.ix.minestom.spleef;

import java.util.Locale;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.kyori.adventure.text.format.TextDecoration;
import net.minestom.server.MinecraftServer;
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent;
import net.minestom.server.event.server.ServerListPingEvent;
import net.minestom.server.ping.Status;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Spleef: players spawn on a floating disc of snow and dig it out from under
 * each other; falling through eliminates you, and the last player standing
 * wins. The server is one persistent {@link Lobby} that starts a fresh
 * {@link Game} — its own instance, event scope, and lifecycle — for every
 * round, so rounds can overlap and a finished arena is simply unregistered.
 */
public final class Main {
    private static final Logger LOGGER = LoggerFactory.getLogger(Main.class);

    public static void main(String[] args) {
        ServerConfig config = ServerConfig.fromEnvironment(System.getenv());
        MinecraftServer server = MinecraftServer.init(config.authentication().minestomAuth());
        MinecraftServer.setBrandName("ix Spleef");

        Lobby lobby = new Lobby(config.minPlayers(), config.countdownSeconds());
        // Every connection lands in the lobby; Game moves players out and back.
        MinecraftServer.getGlobalEventHandler().addListener(AsyncPlayerConfigurationEvent.class, event -> {
            int playerCount = MinecraftServer.getConnectionManager().getOnlinePlayerCount();
            if (playerCount >= config.maxPlayers()) {
                event.getPlayer().kick(Component.text("The Spleef server is full.", NamedTextColor.RED));
                return;
            }
            event.setSpawningInstance(lobby.instance());
            event.getPlayer().setRespawnPoint(Lobby.SPAWN);
        });
        MinecraftServer.getGlobalEventHandler().addListener(ServerListPingEvent.class, event -> event.setStatus(
            Status.builder()
                .description(Component.text("Spleef", NamedTextColor.AQUA, TextDecoration.BOLD)
                    .append(Component.text(" | Fast rounds on Minecraft 26.2", NamedTextColor.GRAY)))
                .playerInfo(MinecraftServer.getConnectionManager().getOnlinePlayerCount(), config.maxPlayers())
                .enforcesSecureChat(config.authentication() == ServerConfig.Authentication.ONLINE)
                .build()));

        server.start(config.bindAddress(), config.port());
        LOGGER.info(
            "spleef server listening on {}:{} with {} authentication",
            config.bindAddress(),
            config.port(),
            config.authentication().name().toLowerCase(Locale.ROOT));
    }

    private Main() {}
}

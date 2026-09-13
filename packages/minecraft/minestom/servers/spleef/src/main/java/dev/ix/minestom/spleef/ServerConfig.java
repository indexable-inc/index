package dev.ix.minestom.spleef;

import java.util.Locale;
import java.util.Map;
import net.minestom.server.Auth;

record ServerConfig(
    String bindAddress,
    int port,
    Authentication authentication,
    int minPlayers,
    int maxPlayers,
    int countdownSeconds
) {
    private static final int DEFAULT_PORT = 25565;
    private static final int DEFAULT_MIN_PLAYERS = 2;
    private static final int DEFAULT_MAX_PLAYERS = 20;
    private static final int DEFAULT_COUNTDOWN_SECONDS = 10;

    enum Authentication {
        ONLINE,
        OFFLINE;

        Auth minestomAuth() {
            return switch (this) {
                case ONLINE -> new Auth.Online();
                case OFFLINE -> new Auth.Offline();
            };
        }
    }

    static ServerConfig fromEnvironment(Map<String, String> environment) {
        String bindAddress = environment.getOrDefault("SPLEEF_BIND_ADDRESS", "0.0.0.0");
        int port = integer(environment, "SPLEEF_PORT", DEFAULT_PORT, 1, 65535);
        int minPlayers = integer(environment, "SPLEEF_MIN_PLAYERS", DEFAULT_MIN_PLAYERS, 2, 64);
        int maxPlayers = integer(environment, "SPLEEF_MAX_PLAYERS", DEFAULT_MAX_PLAYERS, minPlayers, 64);
        int countdownSeconds =
            integer(environment, "SPLEEF_COUNTDOWN_SECONDS", DEFAULT_COUNTDOWN_SECONDS, 1, 300);
        Authentication authentication = authentication(environment.getOrDefault("SPLEEF_AUTH", "online"));
        return new ServerConfig(bindAddress, port, authentication, minPlayers, maxPlayers, countdownSeconds);
    }

    private static Authentication authentication(String value) {
        try {
            return Authentication.valueOf(value.toUpperCase(Locale.ROOT));
        } catch (IllegalArgumentException exception) {
            throw new IllegalArgumentException("SPLEEF_AUTH must be 'online' or 'offline', got: " + value, exception);
        }
    }

    private static int integer(Map<String, String> environment, String name, int defaultValue, int min, int max) {
        String value = environment.get(name);
        if (value == null) return defaultValue;

        final int parsed;
        try {
            parsed = Integer.parseInt(value);
        } catch (NumberFormatException exception) {
            throw new IllegalArgumentException(name + " must be an integer, got: " + value, exception);
        }
        if (parsed < min || parsed > max) {
            throw new IllegalArgumentException(name + " must be between " + min + " and " + max + ", got: " + parsed);
        }
        return parsed;
    }
}

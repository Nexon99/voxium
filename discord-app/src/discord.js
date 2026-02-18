// ═══════════════════════════════════════════════════════
//  Voxium — Discord User API Client (user token flow)
// ═══════════════════════════════════════════════════════
window.VoxiumDiscord = (() => {
    const runtime = window.VOXIUM_RUNTIME_CONFIG || {};
    const apiBase = (runtime.apiBaseUrl || "http://127.0.0.1:8080").replace(/\/$/, "");

    async function parseResponse(response) {
        const text = await response.text();
        let data = null;
        try {
            data = text ? JSON.parse(text) : null;
        } catch (_) {
            data = text;
        }
        if (!response.ok) {
            const message = (data && (data.error || data.message)) || `Erreur HTTP ${response.status}`;
            throw new Error(message);
        }
        return data;
    }

    function getBackendToken() {
        return localStorage.getItem("token") || "";
    }

    /**
     * Proxy a Discord API request through the Voxium backend.
     * The backend uses the stored Discord user token.
     * @param {string} path - Discord API path, e.g. "/users/@me"
     * @param {object} options - { method, body }
     */
    async function request(path, options = {}) {
        const token = getBackendToken();
        if (!token) {
            throw new Error("Session Voxium manquante");
        }

        const method = (options.method || "GET").toUpperCase();
        const body = options.body === undefined ? null : options.body;

        const response = await fetch(`${apiBase}/api/discord/proxy`, {
            method: "POST",
            headers: {
                "Content-Type": "application/json",
                Authorization: `Bearer ${token}`,
            },
            body: JSON.stringify({ method, path, body }),
        });

        return parseResponse(response);
    }

    // ── Convenience helpers ─────────────────────────────

    /** Get current Discord user profile */
    async function getMe() {
        return request("/users/@me");
    }

    /** Get list of guilds the user is in */
    async function getGuilds() {
        return request("/users/@me/guilds");
    }

    /** Get channels for a guild */
    async function getGuildChannels(guildId) {
        return request(`/guilds/${guildId}/channels`);
    }

    /** Get guild info */
    async function getGuild(guildId) {
        return request(`/guilds/${guildId}`);
    }

    /** Get messages in a channel, with optional before cursor */
    async function getMessages(channelId, limit = 50, before = null) {
        let qs = `?limit=${limit}`;
        if (before) qs += `&before=${before}`;
        return request(`/channels/${channelId}/messages${qs}`);
    }

    /** Send a message in a channel */
    async function sendMessage(channelId, content) {
        return request(`/channels/${channelId}/messages`, {
            method: "POST",
            body: { content },
        });
    }

    /** Get user profile (from user ID) */
    async function getUser(userId) {
        return request(`/users/${userId}`);
    }

    /** Get DM channels */
    async function getDMChannels() {
        return request("/users/@me/channels");
    }

    /** Create/open a DM channel */
    async function createDM(recipientId) {
        return request("/users/@me/channels", {
            method: "POST",
            body: { recipient_id: recipientId },
        });
    }

    /** Get guild member */
    async function getGuildMember(guildId, userId) {
        return request(`/guilds/${guildId}/members/${userId}`);
    }

    /** Get guild members (up to 1000, requires GUILD_MEMBERS intent or privileged access) */
    async function getGuildMembers(guildId, limit = 100) {
        return request(`/guilds/${guildId}/members?limit=${limit}`);
    }

    /** Get pinned messages in a channel */
    async function getPinnedMessages(channelId) {
        return request(`/channels/${channelId}/pins`);
    }

    /** Get a single channel by ID */
    async function getChannel(channelId) {
        return request(`/channels/${channelId}`);
    }

    /** Add a reaction to a message */
    async function addReaction(channelId, messageId, emoji) {
        return request(`/channels/${channelId}/messages/${messageId}/reactions/${encodeURIComponent(emoji)}/@me`, {
            method: "PUT",
        });
    }

    /** Delete own reaction */
    async function removeReaction(channelId, messageId, emoji) {
        return request(`/channels/${channelId}/messages/${messageId}/reactions/${encodeURIComponent(emoji)}/@me`, {
            method: "DELETE",
        });
    }

    /** Get user settings (contains guild_folders for guild ordering) */
    async function getUserSettings() {
        return request("/users/@me/settings");
    }

    return {
        request,
        getMe,
        getGuilds,
        getGuild,
        getGuildChannels,
        getMessages,
        sendMessage,
        getUser,
        getDMChannels,
        createDM,
        getGuildMember,
        getGuildMembers,
        getPinnedMessages,
        getChannel,
        addReaction,
        removeReaction,
        getUserSettings,
    };
})();

// ═══════════════════════════════════════════════════════
//  Voxium — Discord Voice Client (WebRTC mode)
// ═══════════════════════════════════════════════════════
//
// Connects to Discord Voice Gateway (wss://{endpoint}?v=8)
// using WebRTC protocol. The backend handles the main Discord
// Gateway to obtain voice server info (token, endpoint, session_id).
//
// Flow:
//   1. POST /api/discord/voice/join  → { token, endpoint, session_id, user_id }
//   2. Connect to Voice Gateway WS
//   3. Identify (op 0)  → receive Ready (op 2) + Hello (op 8)
//   4. Create RTCPeerConnection, generate SDP offer
//   5. Select Protocol (op 1) with protocol:"webrtc" + SDP
//   6. Receive Session Description (op 4) with answer SDP
//   7. Apply answer SDP → WebRTC connection established → audio flows
//
window.VoxiumDiscordVoice = (() => {
    const runtime = window.VOXIUM_RUNTIME_CONFIG || {};
    const apiBase = (runtime.apiBaseUrl || "http://127.0.0.1:8080").replace(/\/$/, "");

    // ── State ───────────────────────────────────────────
    let voiceWs = null;
    let heartbeatInterval = null;
    let heartbeatNonce = null;
    let pc = null;               // RTCPeerConnection
    let localStream = null;      // getUserMedia stream
    let voiceInfo = null;        // { token, endpoint, session_id, user_id, guild_id }
    let connected = false;
    let ssrc = 0;
    let currentGuildId = null;
    let currentChannelId = null;
    let muted = false;
    let deafened = false;

    // Callbacks
    let onStateChange = null;    // (state) => void — "connecting", "connected", "disconnected"
    let onSpeaking = null;       // (userId, ssrc, speaking) => void
    let onError = null;          // (errorMsg) => void

    // ── Public API ──────────────────────────────────────

    function setCallbacks(callbacks = {}) {
        onStateChange = callbacks.onStateChange || null;
        onSpeaking = callbacks.onSpeaking || null;
        onError = callbacks.onError || null;
    }

    /**
     * Join a Discord voice channel.
     * @param {string} guildId
     * @param {string} channelId
     */
    async function joinVoice(guildId, channelId) {
        if (connected || voiceWs) {
            await leaveVoice();
        }

        currentGuildId = guildId;
        currentChannelId = channelId;
        _emitState("connecting");

        try {
            // Step 1: Ask backend to send Update Voice State and return voice server info
            const token = localStorage.getItem("token") || "";
            const resp = await fetch(`${apiBase}/api/discord/voice/join`, {
                method: "POST",
                headers: {
                    "Content-Type": "application/json",
                    Authorization: `Bearer ${token}`,
                },
                body: JSON.stringify({ guild_id: guildId, channel_id: channelId }),
            });

            if (!resp.ok) {
                const err = await resp.json().catch(() => ({}));
                throw new Error(err.error || `HTTP ${resp.status}`);
            }

            voiceInfo = await resp.json();
            console.log("[discord-voice] Voice server info:", voiceInfo);

            if (!voiceInfo.endpoint) {
                throw new Error("No voice endpoint received");
            }

            // Step 2: Get microphone
            try {
                localStream = await navigator.mediaDevices.getUserMedia({
                    audio: {
                        echoCancellation: true,
                        noiseSuppression: true,
                        autoGainControl: true,
                    },
                });
            } catch (micErr) {
                console.warn("[discord-voice] Microphone access denied, joining deaf:", micErr);
                localStream = null;
                deafened = true;
            }

            // Step 3: Connect to Discord Voice Gateway
            // Keep the port from endpoint (it IS the WSS port), only strip :80
            const ep = voiceInfo.endpoint.replace(':80', '');
            const wsUrl = `wss://${ep}/?v=7&encoding=json`;
            console.log("[discord-voice] Connecting to Voice Gateway:", wsUrl);
            console.log("[discord-voice] Voice info:", JSON.stringify(voiceInfo));
            await _connectVoiceGateway(wsUrl);

        } catch (err) {
            console.error("[discord-voice] Join failed:", err);
            _emitError(err.message || "Failed to join voice");
            _emitState("disconnected");
            _cleanup();
        }
    }

    /**
     * Leave the current Discord voice channel.
     */
    async function leaveVoice() {
        if (currentGuildId) {
            try {
                const token = localStorage.getItem("token") || "";
                await fetch(`${apiBase}/api/discord/voice/leave`, {
                    method: "POST",
                    headers: {
                        "Content-Type": "application/json",
                        Authorization: `Bearer ${token}`,
                    },
                    body: JSON.stringify({ guild_id: currentGuildId }),
                });
            } catch (e) {
                console.warn("[discord-voice] Leave request failed:", e);
            }
        }
        _cleanup();
        _emitState("disconnected");
    }

    function toggleMute() {
        muted = !muted;
        if (localStream) {
            localStream.getAudioTracks().forEach(t => { t.enabled = !muted && !deafened; });
        }
        return muted;
    }

    function toggleDeafen() {
        deafened = !deafened;
        if (localStream) {
            localStream.getAudioTracks().forEach(t => { t.enabled = !muted && !deafened; });
        }
        // Also mute/unmute incoming audio
        const audioEls = document.querySelectorAll(".discord-voice-audio");
        audioEls.forEach(el => { el.muted = deafened; });
        return deafened;
    }

    function isConnected() { return connected; }
    function isMuted() { return muted; }
    function isDeafened() { return deafened; }
    function getGuildId() { return currentGuildId; }
    function getChannelId() { return currentChannelId; }

    // ── Voice Gateway WebSocket ─────────────────────────

    function _connectVoiceGateway(wsUrl) {
        return new Promise((resolve, reject) => {
            let resolved = false;

            voiceWs = new WebSocket(wsUrl);

            voiceWs.onopen = () => {
                console.log("[discord-voice] Voice Gateway connected, sending Identify");
                const identifyPayload = {
                    op: 0,
                    d: {
                        server_id: voiceInfo.guild_id || currentGuildId,
                        user_id: voiceInfo.user_id,
                        session_id: voiceInfo.session_id,
                        token: voiceInfo.token,
                        video: false,
                        streams: [],
                    },
                };
                console.log("[discord-voice] Identify payload:", JSON.stringify(identifyPayload));
                _wsSend(identifyPayload);
            };

            voiceWs.onmessage = async (event) => {
                let payload;
                try {
                    payload = JSON.parse(event.data);
                } catch {
                    return;
                }

                const op = payload.op;
                const d = payload.d;

                switch (op) {
                    // Hello (op 8) — start heartbeating
                    case 8: {
                        const interval = d?.heartbeat_interval || 13750;
                        _startHeartbeat(interval);
                        break;
                    }

                    // Ready (op 2) — contains SSRC, IP, port, modes
                    case 2: {
                        console.log("[discord-voice] Voice Ready:", JSON.stringify(d));
                        ssrc = d.ssrc;
                        console.log("[discord-voice] Assigned SSRC:", ssrc);

                        // Now create an RTCPeerConnection and generate SDP offer
                        try {
                            await _setupWebRTC(d);
                            if (!resolved) { resolved = true; resolve(); }
                        } catch (err) {
                            if (!resolved) { resolved = true; reject(err); }
                        }
                        break;
                    }

                    // Session Description (op 4) — answer SDP
                    case 4: {
                        console.log("[discord-voice] Session Description received:", JSON.stringify(d));
                        try {
                            await _handleSessionDescription(d);
                            connected = true;
                            _emitState("connected");
                        } catch (err) {
                            console.error("[discord-voice] Failed to apply session description:", err);
                            _emitError("Échec de la connexion audio WebRTC");
                        }
                        break;
                    }

                    // Speaking (op 5)
                    case 5: {
                        if (onSpeaking) {
                            onSpeaking(d.user_id, d.ssrc, d.speaking);
                        }
                        break;
                    }

                    // Heartbeat ACK (op 6)
                    case 6: {
                        // OK
                        break;
                    }

                    // Resumed (op 9)
                    case 9: {
                        console.log("[discord-voice] Resumed");
                        break;
                    }

                    // Hello (already handled)
                    // Client Disconnect (op 13)
                    case 13: {
                        console.log("[discord-voice] Client disconnected:", d);
                        break;
                    }

                    default:
                        console.log("[discord-voice] Unhandled voice op:", op, JSON.stringify(d));
                }
            };

            voiceWs.onerror = (err) => {
                console.error("[discord-voice] Voice Gateway error:", err);
                console.error("[discord-voice] WS readyState:", voiceWs?.readyState);
                if (!resolved) { resolved = true; reject(new Error("Voice Gateway connection error")); }
            };

            voiceWs.onclose = (ev) => {
                console.log("[discord-voice] Voice Gateway closed: code=", ev.code, "reason=", ev.reason, "wasClean=", ev.wasClean);
                if (!resolved) { resolved = true; reject(new Error(`Voice Gateway closed (code ${ev.code}: ${ev.reason || 'no reason'})`)); }
                if (connected) {
                    connected = false;
                    _cleanup();
                    _emitState("disconnected");
                }
            };
        });
    }

    function _wsSend(payload) {
        if (voiceWs && voiceWs.readyState === WebSocket.OPEN) {
            voiceWs.send(JSON.stringify(payload));
        }
    }

    function _startHeartbeat(intervalMs) {
        _stopHeartbeat();
        heartbeatInterval = setInterval(() => {
            heartbeatNonce = Date.now();
            _wsSend({ op: 3, d: heartbeatNonce });
        }, intervalMs);
        // Send initial heartbeat
        heartbeatNonce = Date.now();
        _wsSend({ op: 3, d: heartbeatNonce });
    }

    function _stopHeartbeat() {
        if (heartbeatInterval) {
            clearInterval(heartbeatInterval);
            heartbeatInterval = null;
        }
    }

    // ── WebRTC Setup ────────────────────────────────────

    async function _setupWebRTC(readyData) {
        // readyData contains: ssrc, ip, port, modes, experiments
        // We use WebRTC mode (protocol: "webrtc")

        const iceServers = (runtime.iceServers || []).concat([
            { urls: "stun:stun.l.google.com:19302" },
        ]);

        pc = new RTCPeerConnection({ iceServers, bundlePolicy: "max-bundle" });

        // Add microphone track
        if (localStream) {
            localStream.getAudioTracks().forEach(track => {
                pc.addTrack(track, localStream);
            });
            // Apply initial mute state
            if (muted || deafened) {
                localStream.getAudioTracks().forEach(t => { t.enabled = false; });
            }
        } else {
            // No mic access — add a silent audio track so SDP has audio
            const ctx = new AudioContext();
            const oscillator = ctx.createOscillator();
            const dest = ctx.createMediaStreamDestination();
            oscillator.connect(dest);
            oscillator.start();
            const silentTrack = dest.stream.getAudioTracks()[0];
            silentTrack.enabled = false;
            pc.addTrack(silentTrack, dest.stream);
        }

        // Handle incoming audio tracks
        pc.ontrack = (event) => {
            console.log("[discord-voice] Remote track received:", event.track.kind);
            if (event.track.kind === "audio") {
                const audio = new Audio();
                audio.className = "discord-voice-audio";
                audio.srcObject = event.streams[0] || new MediaStream([event.track]);
                audio.autoplay = true;
                audio.muted = deafened;
                audio.volume = 1.0;
                document.body.appendChild(audio);
            }
        };

        pc.oniceconnectionstatechange = () => {
            console.log("[discord-voice] ICE state:", pc.iceConnectionState);
        };

        // Create SDP offer
        const offer = await pc.createOffer({
            offerToReceiveAudio: true,
            offerToReceiveVideo: false,
        });

        // Modify SDP to set SSRC if needed
        // Discord expects our SSRC from the Ready event
        let sdp = offer.sdp;

        // Inject our SSRC into the SDP
        // Replace the existing SSRC with the one from Discord
        sdp = _rewriteSdpSsrc(sdp, ssrc);

        await pc.setLocalDescription({ type: "offer", sdp });

        // Wait for ICE gathering to complete (or timeout)
        const finalSdp = await _waitForIceGathering(pc);

        // Send Select Protocol (op 1) with WebRTC
        _wsSend({
            op: 1,
            d: {
                protocol: "webrtc",
                data: finalSdp,
                rtc_connection_id: _generateRtcConnectionId(),
                codecs: [
                    { name: "opus", type: "audio", priority: 1000, payload_type: 120 },
                ],
            },
        });

        // Send Speaking (op 5) to indicate we're ready
        _wsSend({
            op: 5,
            d: {
                speaking: muted ? 0 : 1,
                delay: 0,
                ssrc: ssrc,
            },
        });
    }

    function _rewriteSdpSsrc(sdp, targetSsrc) {
        // Replace all SSRC values in the SDP with our assigned SSRC
        const lines = sdp.split("\r\n");
        const newLines = [];
        let replaced = false;
        for (const line of lines) {
            if (line.startsWith("a=ssrc:")) {
                if (!replaced) {
                    // Replace the SSRC value
                    const parts = line.split(" ");
                    parts[0] = `a=ssrc:${targetSsrc}`;
                    newLines.push(parts.join(" "));
                    replaced = false; // allow multiple a=ssrc lines
                } else {
                    const parts = line.split(" ");
                    parts[0] = `a=ssrc:${targetSsrc}`;
                    newLines.push(parts.join(" "));
                }
            } else {
                newLines.push(line);
            }
        }
        return newLines.join("\r\n");
    }

    function _waitForIceGathering(peerConnection) {
        return new Promise((resolve) => {
            if (peerConnection.iceGatheringState === "complete") {
                resolve(peerConnection.localDescription.sdp);
                return;
            }

            const timeout = setTimeout(() => {
                resolve(peerConnection.localDescription.sdp);
            }, 3000);

            peerConnection.onicegatheringstatechange = () => {
                if (peerConnection.iceGatheringState === "complete") {
                    clearTimeout(timeout);
                    resolve(peerConnection.localDescription.sdp);
                }
            };
        });
    }

    async function _handleSessionDescription(data) {
        // Discord Voice Gateway returns a SIMPLIFIED SDP (proprietary format
        // like "m=audio 19333 ICE/SDP") that is NOT a valid WebRTC SDP answer.
        // We must construct a proper SDP answer from Discord's info + our offer.
        if (!pc) throw new Error("No peer connection");

        const discordSdp = data.sdp;
        if (!discordSdp) {
            console.warn("[discord-voice] No SDP in session description, data:", data);
            connected = true;
            _emitState("connected");
            return;
        }

        console.log("[discord-voice] Discord raw SDP:", discordSdp);

        // Parse Discord's simplified SDP
        const lines = discordSdp.split("\n").map(l => l.trim()).filter(l => l);
        let remoteIp = "0.0.0.0";
        let remotePort = 0;
        let iceUfrag = "";
        let icePwd = "";
        let fingerprint = "";
        const candidates = [];

        for (const line of lines) {
            if (line.startsWith("c=IN IP4 ")) {
                remoteIp = line.substring("c=IN IP4 ".length).trim();
            } else if (line.startsWith("m=audio ")) {
                remotePort = parseInt(line.split(" ")[1], 10) || 0;
            } else if (line.startsWith("a=ice-ufrag:")) {
                iceUfrag = line.substring("a=ice-ufrag:".length).trim();
            } else if (line.startsWith("a=ice-pwd:")) {
                icePwd = line.substring("a=ice-pwd:".length).trim();
            } else if (line.startsWith("a=fingerprint:")) {
                fingerprint = line.substring("a=fingerprint:".length).trim();
            } else if (line.startsWith("a=candidate:")) {
                candidates.push(line);
            }
        }

        console.log("[discord-voice] Parsed Discord SDP — ip:", remoteIp, "port:", remotePort, "ufrag:", iceUfrag);

        // Extract the 'mid' from our local offer SDP
        const localSdp = pc.localDescription?.sdp || "";
        const midMatch = localSdp.match(/a=mid:(\S+)/);
        const mid = midMatch ? midMatch[1] : "0";

        // Extract the opus payload type from our local offer
        // Chrome typically uses PT 111 for opus, but it can vary
        const opusMatch = localSdp.match(/a=rtpmap:(\d+)\s+opus\/48000\/2/);
        const opusPT = opusMatch ? opusMatch[1] : "111";

        // Extract the corresponding fmtp line for opus from local offer
        const fmtpMatch = localSdp.match(new RegExp(`a=fmtp:${opusPT}\\s+(.+)`));
        const opusFmtp = fmtpMatch ? fmtpMatch[1] : "minptime=10;useinbandfec=1";

        // Use UDP/TLS/RTP/SAVPF as protocol (matches DTLS-SRTP)
        const proto = "UDP/TLS/RTP/SAVPF";

        // Build a complete SDP answer — only declare opus codec
        const answerLines = [
            "v=0",
            "o=- 0 0 IN IP4 0.0.0.0",
            "s=-",
            "t=0 0",
            `a=group:BUNDLE ${mid}`,
            "a=msid-semantic: WMS *",
            `m=audio ${remotePort} ${proto} ${opusPT}`,
            `c=IN IP4 ${remoteIp}`,
            `a=rtcp:${remotePort}`,
            `a=ice-ufrag:${iceUfrag}`,
            `a=ice-pwd:${icePwd}`,
            `a=fingerprint:${fingerprint}`,
            "a=setup:active",
            `a=mid:${mid}`,
            "a=sendrecv",
            "a=rtcp-mux",
            `a=rtpmap:${opusPT} opus/48000/2`,
            `a=fmtp:${opusPT} ${opusFmtp}`,
        ];

        // Add candidates
        for (const c of candidates) {
            answerLines.push(c);
        }

        // End with empty line
        answerLines.push("");

        const fullSdp = answerLines.join("\r\n");
        console.log("[discord-voice] Constructed SDP answer:\n", fullSdp);

        await pc.setRemoteDescription({ type: "answer", sdp: fullSdp });
        console.log("[discord-voice] Remote description applied successfully");
    }

    function _generateRtcConnectionId() {
        // Generate a UUID-like string
        return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (c) => {
            const r = (Math.random() * 16) | 0;
            const v = c === "x" ? r : (r & 0x3) | 0x8;
            return v.toString(16);
        });
    }

    // ── Cleanup ─────────────────────────────────────────

    function _cleanup() {
        _stopHeartbeat();

        if (voiceWs) {
            try { voiceWs.close(); } catch (_) { }
            voiceWs = null;
        }

        if (pc) {
            try { pc.close(); } catch (_) { }
            pc = null;
        }

        if (localStream) {
            localStream.getTracks().forEach(t => t.stop());
            localStream = null;
        }

        // Remove any audio elements we added
        document.querySelectorAll(".discord-voice-audio").forEach(el => el.remove());

        connected = false;
        voiceInfo = null;
        ssrc = 0;
        currentGuildId = null;
        currentChannelId = null;
        muted = false;
        deafened = false;
    }

    function _emitState(state) {
        console.log("[discord-voice] State:", state);
        if (onStateChange) onStateChange(state);
    }

    function _emitError(msg) {
        console.error("[discord-voice] Error:", msg);
        if (onError) onError(msg);
    }

    // ── Exports ─────────────────────────────────────────
    return {
        setCallbacks,
        joinVoice,
        leaveVoice,
        toggleMute,
        toggleDeafen,
        isConnected,
        isMuted,
        isDeafened,
        getGuildId,
        getChannelId,
    };
})();

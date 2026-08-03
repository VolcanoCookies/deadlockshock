(function () {
    "use strict";

    var LOG_PREFIX = "[DEADLOCK_DEATH_HOOK]";
    var POLL_INTERVAL_SECONDS = 0.1;
    var context = $.GetContextPanel();
    var localPlayerPanel = null;
    var baselineEstablished = false;
    var wasDead = false;
    var deathSequence = 0;
    var sessionId = Date.now().toString(36) + "-" + Math.floor(Math.random() * 0x1000000).toString(36);

    function emit(eventName, fields) {
        var payload = {
            schema: 1,
            event: eventName,
            session_id: sessionId,
            client_time_ms: Date.now()
        };

        if (fields) {
            for (var key in fields) {
                if (Object.prototype.hasOwnProperty.call(fields, key)) {
                    payload[key] = fields[key];
                }
            }
        }

        $.Msg(LOG_PREFIX + JSON.stringify(payload));
    }

    function findLocalPlayerPanel() {
        if (localPlayerPanel && localPlayerPanel.IsValid && localPlayerPanel.IsValid()) {
            return localPlayerPanel;
        }

        localPlayerPanel = null;
        baselineEstablished = false;

        var panels = context.FindChildrenWithClassTraverse("LocalPlayer");
        for (var i = 0; i < panels.length; i++) {
            if (panels[i].paneltype === "CitadelHudTopBarPlayer") {
                localPlayerPanel = panels[i];
                return localPlayerPanel;
            }
        }

        return null;
    }

    function pollDeathState() {
        if (!context || !context.IsValid || !context.IsValid()) {
            return;
        }

        var panel = findLocalPlayerPanel();
        if (panel) {
            var isDead = panel.BHasClass("Dead");

            if (!baselineEstablished) {
                wasDead = isDead;
                baselineEstablished = true;
            } else {
                if (isDead && !wasDead) {
                    deathSequence++;
                    emit("local_player_death", {
                        sequence: deathSequence,
                        detection: "top_bar_local_player_dead_class"
                    });
                }

                wasDead = isDead;
            }
        }

        $.Schedule(POLL_INTERVAL_SECONDS, pollDeathState);
    }

    emit("hook_ready", {
        poll_interval_ms: POLL_INTERVAL_SECONDS * 1000
    });
    pollDeathState();
})();

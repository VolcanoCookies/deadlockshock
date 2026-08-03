import { afterEach, describe, expect, test } from "bun:test";

const source = await Bun.file("mod/panorama/scripts/death_http_bridge.js").text();
const originalDateNow = Date.now;
const originalMathRandom = Math.random;

function createHarness({ initiallyDead = false, playerAvailable = true } = {}) {
    let dead = initiallyDead;
    let available = playerAvailable;
    let playerValid = true;
    let contextValid = true;
    let now = 1_700_000_000_000;
    const scheduled = [];
    const messages = [];

    const player = {
        paneltype: "CitadelHudTopBarPlayer",
        IsValid: () => playerValid,
        BHasClass: (className) => className === "Dead" && dead,
    };

    const context = {
        IsValid: () => contextValid,
        FindChildrenWithClassTraverse: (className) =>
            className === "LocalPlayer" && available ? [player] : [],
    };

    const panorama = {
        GetContextPanel: () => context,
        Msg: (message) => messages.push(message),
        Schedule: (delay, callback) => scheduled.push({ delay, callback }),
    };

    Date.now = () => now++;
    Math.random = () => 0.5;
    new Function("$", source)(panorama);

    function runNextPoll() {
        const next = scheduled.shift();
        expect(next).toBeDefined();
        expect(next.delay).toBe(0.1);
        next.callback();
    }

    function events() {
        return messages.map((message) => {
            expect(message.startsWith("[DEADLOCK_DEATH_HOOK]")).toBe(true);
            return JSON.parse(message.slice("[DEADLOCK_DEATH_HOOK]".length));
        });
    }

    return {
        events,
        runNextPoll,
        setDead: (value) => { dead = value; },
        setAvailable: (value) => { available = value; },
        invalidatePlayer: () => { playerValid = false; },
        invalidateContext: () => { contextValid = false; },
        scheduled,
    };
}

afterEach(() => {
    Date.now = originalDateNow;
    Math.random = originalMathRandom;
});

describe("death_http_bridge", () => {
    test("emits a versioned ready event", () => {
        const harness = createHarness();

        expect(harness.events()).toEqual([
            expect.objectContaining({
                schema: 1,
                event: "hook_ready",
                session_id: expect.any(String),
                client_time_ms: expect.any(Number),
                poll_interval_ms: 100,
            }),
        ]);
    });

    test("emits once for each alive-to-dead transition", () => {
        const harness = createHarness();

        harness.setDead(true);
        harness.runNextPoll();
        harness.runNextPoll();
        harness.setDead(false);
        harness.runNextPoll();
        harness.setDead(true);
        harness.runNextPoll();

        const deaths = harness.events().filter((event) => event.event === "local_player_death");
        expect(deaths).toEqual([
            expect.objectContaining({
                schema: 1,
                sequence: 1,
                detection: "top_bar_local_player_dead_class",
            }),
            expect.objectContaining({
                schema: 1,
                sequence: 2,
                detection: "top_bar_local_player_dead_class",
            }),
        ]);
        expect(deaths[0].session_id).toBe(deaths[1].session_id);
    });

    test("does not emit when loaded while already dead", () => {
        const harness = createHarness({ initiallyDead: true });

        harness.runNextPoll();

        expect(harness.events().filter((event) => event.event === "local_player_death")).toHaveLength(0);
    });

    test("establishes a baseline when the local panel appears", () => {
        const harness = createHarness({ initiallyDead: true, playerAvailable: false });

        harness.runNextPoll();
        harness.setAvailable(true);
        harness.runNextPoll();

        expect(harness.events().filter((event) => event.event === "local_player_death")).toHaveLength(0);
    });

    test("stops polling after its Panorama context is destroyed", () => {
        const harness = createHarness();

        harness.invalidateContext();
        harness.runNextPoll();

        expect(harness.scheduled).toHaveLength(0);
    });
});

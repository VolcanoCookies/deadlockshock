import { afterEach, describe, expect, test } from "bun:test";

const source = await Bun.file("mod/panorama/scripts/death_http_bridge.js").text();
const originalDateNow = Date.now;
const originalMathRandom = Math.random;

function createPanel({
    id = "",
    paneltype = "Panel",
    classes = [],
    text = null,
    children = [],
    attributes = {},
    properties = {},
} = {}) {
    let parent = null;
    let valid = true;
    let classNames = new Set(classes);
    let childPanels = children;
    const attributeValues = { ...attributes };
    const panel = {
        id,
        paneltype,
        visible: true,
        enabled: true,
        text,
        style: {},
        ...properties,
        IsValid: () => valid,
        GetParent: () => parent,
        Children: () => childPanels,
        BHasClass: (className) => classNames.has(className),
        GetAttributeString: (name, fallback) =>
            Object.hasOwn(attributeValues, name) ? attributeValues[name] : fallback,
        FindChildTraverse: (wantedId) => {
            for (const child of childPanels) {
                if (child.id === wantedId) {
                    return child;
                }
                const nested = child.FindChildTraverse(wantedId);
                if (nested) {
                    return nested;
                }
            }
            return null;
        },
        FindChildrenWithClassTraverse: (className) => {
            const matches = [];
            for (const child of childPanels) {
                if (child.BHasClass(className)) {
                    matches.push(child);
                }
                matches.push(...child.FindChildrenWithClassTraverse(className));
            }
            return matches;
        },
        setAttribute: (name, value) => {
            if (value === null) {
                delete attributeValues[name];
            } else {
                attributeValues[name] = value;
            }
        },
        setChildren: (nextChildren) => {
            childPanels = nextChildren;
            for (const child of childPanels) {
                child.setParent(panel);
            }
        },
        setClasses: (nextClasses) => { classNames = new Set(nextClasses); },
        setParent: (nextParent) => { parent = nextParent; },
        setText: (nextText) => { panel.text = nextText; },
        setValid: (nextValid) => { valid = nextValid; },
    };
    panel.setChildren(childPanels);
    return panel;
}

function createAbilityEntry({
    identity = "ability_test",
    name = "Test Ability",
    classes = ["trained", "Tier0"],
    charges = 3,
    maxCharges = 3,
} = {}) {
    const abilityName = createPanel({ classes: ["ability_name"], paneltype: "Label", text: name });
    const cooldownTimer = createPanel({ classes: ["cooldown_timer"], paneltype: "Label", text: "" });
    const chargeProgress = createPanel({
        paneltype: "ProgressBarWithMiddle",
        properties: { lowervalue: charges, uppervalue: charges, max: maxCharges },
    });
    const chargeContainer = createPanel({ classes: ["stack_charges"], children: [chargeProgress] });
    const entry = createPanel({
        classes: ["ability_container", ...classes],
        attributes: { ability_id: identity, ability_slot: "signature_1" },
        children: [abilityName, cooldownTimer, chargeContainer],
    });
    return { abilityName, chargeContainer, chargeProgress, cooldownTimer, entry };
}

function createHarness({
    initiallyDead = false,
    playerAvailable = true,
    abilityAvailable = true,
    ability = {},
} = {}) {
    let dead = initiallyDead;
    let available = playerAvailable;
    let rootAvailable = abilityAvailable;
    let contextValid = true;
    let now = 1_700_000_000_000;
    const scheduled = [];
    const messages = [];

    let player = createPanel({ paneltype: "CitadelHudTopBarPlayer" });
    player.BHasClass = (className) => className === "Dead" && dead;

    let abilityParts = createAbilityEntry(ability);
    const abilityRoot = createPanel({
        id: "hud_signature",
        paneltype: "CitadelHudAbilities",
        attributes: { hero_id: "hero_a" },
        children: [abilityParts.entry],
    });
    const hudRoot = createPanel({ id: "HudCore", children: [abilityRoot] });

    const context = createPanel();
    context.GetParent = () => rootAvailable ? hudRoot : null;
    context.FindChildrenWithClassTraverse = (className) =>
        className === "LocalPlayer" && available ? [player] : [];
    context.IsValid = () => contextValid;

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

    function events(name) {
        const parsed = messages
            .filter((message) => message.startsWith("[DEADLOCK_DEATH_HOOK]"))
            .map((message) => JSON.parse(message.slice("[DEADLOCK_DEATH_HOOK]".length)));
        return name ? parsed.filter((event) => event.event === name) : parsed;
    }

    function setAbilityState({
        classes,
        charges,
        maxCharges,
        uppervalue,
        identity,
        name,
        cooldownText,
    }) {
        if (classes !== undefined) {
            abilityParts.entry.setClasses(["ability_container", ...classes]);
        }
        if (charges !== undefined) {
            abilityParts.chargeProgress.lowervalue = charges;
        }
        if (maxCharges !== undefined) {
            abilityParts.chargeProgress.max = maxCharges;
        }
        if (uppervalue !== undefined) {
            abilityParts.chargeProgress.uppervalue = uppervalue;
        }
        if (identity !== undefined) {
            abilityParts.entry.setAttribute("ability_id", identity);
        }
        if (name !== undefined) {
            abilityParts.abilityName.setText(name);
        }
        if (cooldownText !== undefined) {
            abilityParts.cooldownTimer.setText(cooldownText);
        }
    }

    return {
        events,
        runNextPoll,
        scheduled,
        setAbilityState,
        setAbilityComplete: (complete) => {
            abilityParts.chargeContainer.setChildren(complete ? [abilityParts.chargeProgress] : []);
        },
        setAbilityAvailable: (value) => { rootAvailable = value; },
        setAvailable: (value) => { available = value; },
        setDead: (value) => { dead = value; },
        setHeroIdentity: (value) => { abilityRoot.setAttribute("hero_id", value); },
        replaceAbilityPanel: (nextAbility = {}) => {
            abilityParts = createAbilityEntry(nextAbility);
            abilityRoot.setChildren([abilityParts.entry]);
        },
        replacePlayer: () => {
            player.setValid(false);
            player = createPanel({ paneltype: "CitadelHudTopBarPlayer" });
            player.BHasClass = (className) => className === "Dead" && dead;
        },
        invalidateContext: () => { contextValid = false; },
    };
}

function actionable(harness) {
    return harness.events().filter((event) => event.event !== "hook_ready");
}

function chargedClasses(...extra) {
    return ["trained", "Tier0", "has_stack_charges", ...extra];
}

afterEach(() => {
    Date.now = originalDateNow;
    Math.random = originalMathRandom;
});

describe("death_http_bridge", () => {
    test("emits only the schema-1 diagnostic ready envelope on startup", () => {
        const harness = createHarness();

        expect(harness.events()).toEqual([{
            schema: 1,
            event: "hook_ready",
            session_id: expect.any(String),
            client_time_ms: expect.any(Number),
            poll_interval_ms: 100,
        }]);
    });

    test("uses one global sequence for interleaved ability and death events", () => {
        const harness = createHarness();
        harness.setAbilityState({ classes: ["trained", "Tier0", "cooling_down"] });
        harness.runNextPoll();
        harness.setDead(true);
        harness.runNextPoll();
        harness.setAbilityState({ classes: ["trained", "Tier0"] });
        harness.setDead(false);
        harness.runNextPoll();
        harness.setAbilityState({ classes: ["trained", "Tier0", "active"] });
        harness.runNextPoll();

        expect(actionable(harness).map(({ event, sequence }) => ({ event, sequence }))).toEqual([
            { event: "ability_used", sequence: 1 },
            { event: "local_player_death", sequence: 2 },
            { event: "ability_used", sequence: 3 },
        ]);
    });

    test("four charged decrements emit four exact use payloads while upper progress is ignored", () => {
        const harness = createHarness({
            ability: { classes: chargedClasses(), charges: 4, maxCharges: 4 },
        });

        for (let charges = 3; charges >= 0; charges--) {
            harness.setAbilityState({ charges, uppervalue: charges + 0.75 });
            harness.runNextPoll();
        }

        expect(harness.events("ability_used")).toEqual([3, 2, 1, 0].map((charges, index) => ({
            schema: 1,
            event: "ability_used",
            session_id: expect.any(String),
            client_time_ms: expect.any(Number),
            sequence: index + 1,
            ability_slot: 1,
            ability_name: "Test Ability",
            detection: "charge_decrement",
            charges_before: charges + 1,
            charges_after: charges,
        })));
    });

    test("charged cooldown completion and later charge restoration are separate readiness events", () => {
        const harness = createHarness({
            ability: { classes: chargedClasses("cooling_down"), charges: 1, maxCharges: 3 },
        });
        harness.setAbilityState({ classes: chargedClasses(), charges: 1 });
        harness.runNextPoll();
        harness.setAbilityState({ charges: 2, uppervalue: 1.25 });
        harness.runNextPoll();

        expect(harness.events("ability_cooldown_ready")).toEqual([
            expect.objectContaining({
                sequence: 1,
                detection: "cooldown_finished",
                ability_slot: 1,
            }),
            expect.objectContaining({
                sequence: 2,
                detection: "charge_restored",
                charges_before: 1,
                charges_after: 2,
            }),
        ]);
    });

    test("coalesces coincident charged readiness causes into one event", () => {
        const harness = createHarness({
            ability: { classes: chargedClasses("cooling_down"), charges: 1, maxCharges: 3 },
        });
        harness.setAbilityState({ classes: chargedClasses(), charges: 2 });
        harness.runNextPoll();

        expect(harness.events("ability_cooldown_ready")).toEqual([
            expect.objectContaining({
                sequence: 1,
                detection: "cooldown_finished_and_charge_restored",
                charges_before: 1,
                charges_after: 2,
            }),
        ]);
    });

    test("coalesces non-charged active and cooling signals and detects either independently", () => {
        const simultaneous = createHarness();
        simultaneous.setAbilityState({ classes: ["trained", "Tier0", "active", "cooling_down"] });
        simultaneous.runNextPoll();
        expect(simultaneous.events("ability_used")).toEqual([
            expect.objectContaining({ detection: "cooldown_started_and_activated", sequence: 1 }),
        ]);

        const active = createHarness();
        active.setAbilityState({ classes: ["trained", "Tier0", "active"] });
        active.runNextPoll();
        expect(active.events("ability_used")).toEqual([
            expect.objectContaining({ detection: "activated", sequence: 1 }),
        ]);

        const cooldown = createHarness();
        cooldown.setAbilityState({ classes: ["trained", "Tier0", "cooling_down"] });
        cooldown.runNextPoll();
        expect(cooldown.events("ability_used")).toEqual([
            expect.objectContaining({ detection: "cooldown_started", sequence: 1 }),
        ]);
    });

    test("detects non-charged cooldown readiness", () => {
        const harness = createHarness({ ability: { classes: ["trained", "Tier0", "cooling_down"] } });
        harness.setAbilityState({ classes: ["trained", "Tier0"] });
        harness.runNextPoll();

        expect(harness.events("ability_cooldown_ready")).toEqual([
            expect.objectContaining({ detection: "cooldown_finished", sequence: 1 }),
        ]);
    });

    test("charged active and cooling signals do not substitute for a charge decrement", () => {
        const harness = createHarness({
            ability: { classes: chargedClasses(), charges: 2, maxCharges: 3 },
        });
        harness.setAbilityState({ classes: chargedClasses("active", "cooling_down"), charges: 2 });
        harness.runNextPoll();

        expect(harness.events("ability_used")).toHaveLength(0);
    });

    test("initial full charges and upgrade-driven tier, maximum, and charge increases rebaseline", () => {
        const harness = createHarness({
            ability: { classes: chargedClasses(), charges: 3, maxCharges: 3 },
        });
        harness.setAbilityState({ uppervalue: 2.5 });
        harness.runNextPoll();
        harness.setAbilityState({
            classes: ["trained", "Tier1", "has_stack_charges"],
            charges: 4,
            maxCharges: 4,
        });
        harness.runNextPoll();
        harness.runNextPoll();

        expect(actionable(harness)).toHaveLength(0);
    });

    test("hero and non-localized ability identity changes rebaseline", () => {
        const harness = createHarness();
        harness.setHeroIdentity("hero_b");
        harness.setAbilityState({ classes: ["trained", "Tier0", "active"] });
        harness.runNextPoll();
        harness.setAbilityState({ identity: "ability_other", classes: ["trained", "Tier0", "cooling_down"] });
        harness.runNextPoll();

        expect(actionable(harness)).toHaveLength(0);
    });

    test("localized ability name changes rebaseline when no stable identity is exposed", () => {
        const harness = createHarness({ ability: { identity: null, name: "Hero A Ability" } });
        harness.setAbilityState({
            name: "Hero B Ability",
            classes: ["trained", "Tier0", "cooling_down"],
        });
        harness.runNextPoll();

        expect(actionable(harness)).toHaveLength(0);
    });

    test("root and ability panel reacquisition suppress their first snapshots", () => {
        const harness = createHarness();
        harness.setAbilityAvailable(false);
        harness.runNextPoll();
        harness.setAbilityState({ classes: ["trained", "Tier0", "active"] });
        harness.setAbilityAvailable(true);
        harness.runNextPoll();
        harness.replaceAbilityPanel({ classes: ["trained", "Tier0", "cooling_down"] });
        harness.runNextPoll();

        expect(actionable(harness)).toHaveLength(0);
    });

    test("incomplete charged snapshots are ignored and completion establishes a new baseline", () => {
        const harness = createHarness({
            ability: { classes: chargedClasses(), charges: 2, maxCharges: 3 },
        });
        harness.setAbilityComplete(false);
        harness.runNextPoll();
        harness.setAbilityState({ charges: 1 });
        harness.setAbilityComplete(true);
        harness.runNextPoll();

        expect(actionable(harness)).toHaveLength(0);
    });

    test("missing numeric charge properties establish a fresh baseline when restored", () => {
        const harness = createHarness({
            ability: { classes: chargedClasses(), charges: 2, maxCharges: 3 },
        });
        harness.setAbilityState({ charges: null });
        harness.runNextPoll();
        harness.setAbilityState({ charges: 1 });
        harness.runNextPoll();

        expect(actionable(harness)).toHaveLength(0);
    });

    test("death and respawn rebaseline abilities while preserving death behavior", () => {
        const harness = createHarness();
        harness.setAbilityState({ classes: ["trained", "Tier0", "active"] });
        harness.setDead(true);
        harness.runNextPoll();
        harness.setAbilityState({ classes: ["trained", "Tier0", "cooling_down"] });
        harness.runNextPoll();
        harness.setDead(false);
        harness.setAbilityState({ classes: ["trained", "Tier0", "active"] });
        harness.runNextPoll();

        expect(actionable(harness)).toEqual([
            expect.objectContaining({
                event: "local_player_death",
                sequence: 1,
                detection: "top_bar_local_player_dead_class",
            }),
        ]);
    });

    test("rejected targeting and presentation-only changes do not emit", () => {
        const harness = createHarness();
        harness.setAbilityState({
            classes: ["trained", "Tier0", "targeting", "ability_not_ready", "channeling"],
            name: "Localized Other Name",
            cooldownText: "9.7",
        });
        harness.runNextPoll();
        harness.setAbilityState({ cooldownText: "8.2" });
        harness.runNextPoll();

        expect(actionable(harness)).toHaveLength(0);
    });

    test("does not emit death when loaded dead or when a dead player panel is reacquired", () => {
        const startup = createHarness({ initiallyDead: true });
        startup.runNextPoll();
        expect(startup.events("local_player_death")).toHaveLength(0);

        const reacquired = createHarness();
        reacquired.setAvailable(false);
        reacquired.runNextPoll();
        reacquired.setDead(true);
        reacquired.replacePlayer();
        reacquired.setAvailable(true);
        reacquired.runNextPoll();
        expect(reacquired.events("local_player_death")).toHaveLength(0);
    });

    test("emits once for every alive-to-dead transition", () => {
        const harness = createHarness();
        harness.setDead(true);
        harness.runNextPoll();
        harness.runNextPoll();
        harness.setDead(false);
        harness.runNextPoll();
        harness.setDead(true);
        harness.runNextPoll();

        expect(harness.events("local_player_death")).toEqual([
            expect.objectContaining({ sequence: 1, detection: "top_bar_local_player_dead_class" }),
            expect.objectContaining({ sequence: 2, detection: "top_bar_local_player_dead_class" }),
        ]);
    });

    test("stops scheduling after its Panorama context is destroyed", () => {
        const harness = createHarness();
        harness.invalidateContext();
        harness.runNextPoll();
        expect(harness.scheduled).toHaveLength(0);
    });
});

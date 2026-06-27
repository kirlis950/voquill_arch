import { useEffect, useMemo, useRef } from "react";
import type { ActivationController } from "./activation";

type HoldAction = {
  controller: ActivationController;
  combos: string[][];
  triggerCount: number;
  /**
   * Optional explicit key-down counter. When this increments, the
   * controller's `handlePress()` is invoked directly — bypassing the
   * `keysHeld`/toggle inference entirely. Intended for external callers
   * (e.g. an evdev-based bridge daemon) that can observe true hardware
   * press/release events and want genuine hold-to-talk behavior instead
   * of tap-to-lock.
   */
  pressCount?: number;
  /** Optional explicit key-up counter; pairs with {@link pressCount}. */
  releaseCount?: number;
};

export type UseHotkeyHoldManyArgs = {
  actions: HoldAction[];
  keysHeld: string[];
  isDisabled?: boolean;
};

export type UseHotkeyHoldArgs = {
  controller: ActivationController;
  combos: string[][];
  triggerCount: number;
  keysHeld: string[];
  isDisabled?: boolean;
  pressCount?: number;
  releaseCount?: number;
};

export type UseHotkeyFireArgs = {
  combos: string[][];
  triggerCount: number;
  keysHeld: string[];
  isDisabled?: boolean;
  onFire?: () => void;
};

/**
 * Drives one or more {@link ActivationController}s from a press/release model
 * over the supplied keys. The consumer owns all state (held keys, combos per
 * action, trigger counts) and feeds it in; this hook is pure behavior.
 *
 * Controller lifetimes are the consumer's responsibility — this hook does not
 * call `.dispose()`.
 */
export const useHotkeyHoldMany = (args: UseHotkeyHoldManyArgs): void => {
  const isDisabled = Boolean(args.isDisabled);
  const { actions, keysHeld } = args;

  const combosSignature = useMemo(
    () => actions.map((a) => JSON.stringify(a.combos)).join("|"),
    [actions],
  );

  const wasPressedRef = useRef<Map<ActivationController, boolean>>(new Map());

  useEffect(() => {
    const normalize = (key: string) => key.toLowerCase();

    const matchesCombo = (held: string[], combo: string[]) => {
      if (combo.length === 0) {
        return false;
      }

      const uniqueHeld = Array.from(new Set(held.map((key) => normalize(key))));
      const required = Array.from(new Set(combo.map((key) => normalize(key))));

      if (uniqueHeld.length !== required.length) {
        return false;
      }

      const heldSet = new Set(uniqueHeld);
      return required.every((key) => heldSet.has(key));
    };

    for (const action of actions) {
      const availableCombos = action.combos;
      const wasPressed = wasPressedRef.current.get(action.controller) ?? false;
      const isPressed = availableCombos.some((combo) =>
        matchesCombo(keysHeld, combo),
      );

      if (isDisabled) {
        wasPressedRef.current.set(action.controller, isPressed);
        action.controller.reset();
        continue;
      }

      if (
        action.controller.isActive &&
        !wasPressed &&
        !action.controller.hasHadRelease
      ) {
        action.controller.forceReset();
      }

      if (availableCombos.length === 0) {
        wasPressedRef.current.set(action.controller, false);
        action.controller.reset();
        continue;
      }

      if (isPressed && !wasPressed) {
        if (action.controller.shouldIgnoreActivation) {
          wasPressedRef.current.set(action.controller, isPressed);
          continue;
        }

        action.controller.handlePress();
      } else if (!isPressed && wasPressed) {
        action.controller.clearIgnore();
        action.controller.handleRelease();
      }

      wasPressedRef.current.set(action.controller, isPressed);
    }
  }, [keysHeld, combosSignature, actions, isDisabled]);

  const triggerSignature = useMemo(
    () => actions.map((a) => a.triggerCount).join(","),
    [actions],
  );
  const prevTriggerCountsRef = useRef<Map<ActivationController, number>>(
    new Map(),
  );

  useEffect(() => {
    if (!isDisabled) {
      for (const action of actions) {
        const prev =
          prevTriggerCountsRef.current.get(action.controller) ?? 0;
        const curr = action.triggerCount;
        if (curr > prev) {
          action.controller.toggle();
        }
      }
    }
    for (const action of actions) {
      prevTriggerCountsRef.current.set(action.controller, action.triggerCount);
    }
  }, [triggerSignature, isDisabled, actions]);

  // Explicit press/release counters from callers that can observe true
  // hardware key state (e.g. an evdev daemon talking to the bridge server).
  // These call handlePress()/handleRelease() directly, giving genuine
  // hold-to-talk instead of the tap-to-lock behavior of toggle().
  const pressReleaseSignature = useMemo(
    () =>
      actions
        .map((a) => `${a.pressCount ?? 0}:${a.releaseCount ?? 0}`)
        .join(","),
    [actions],
  );
  const prevPressCountsRef = useRef<Map<ActivationController, number>>(
    new Map(),
  );
  const prevReleaseCountsRef = useRef<Map<ActivationController, number>>(
    new Map(),
  );

  useEffect(() => {
    if (!isDisabled) {
      for (const action of actions) {
        const prevPress =
          prevPressCountsRef.current.get(action.controller) ?? 0;
        const currPress = action.pressCount ?? 0;
        if (currPress > prevPress) {
          action.controller.handlePress();
        }

        const prevRelease =
          prevReleaseCountsRef.current.get(action.controller) ?? 0;
        const currRelease = action.releaseCount ?? 0;
        if (currRelease > prevRelease) {
          action.controller.handleRelease();
        }
      }
    }
    for (const action of actions) {
      prevPressCountsRef.current.set(action.controller, action.pressCount ?? 0);
      prevReleaseCountsRef.current.set(
        action.controller,
        action.releaseCount ?? 0,
      );
    }
  }, [pressReleaseSignature, isDisabled, actions]);
};

/**
 * Single-controller variant of {@link useHotkeyHoldMany}.
 */
export const useHotkeyHold = (args: UseHotkeyHoldArgs): void => {
  const actions = useMemo(
    () => [
      {
        controller: args.controller,
        combos: args.combos,
        triggerCount: args.triggerCount,
        pressCount: args.pressCount,
        releaseCount: args.releaseCount,
      },
    ],
    [
      args.controller,
      args.combos,
      args.triggerCount,
      args.pressCount,
      args.releaseCount,
    ],
  );
  useHotkeyHoldMany({
    actions,
    keysHeld: args.keysHeld,
    isDisabled: args.isDisabled,
  });
};

/**
 * Fires `onFire` on a press-then-release (tap) that matches one of the combos,
 * and also when `triggerCount` increments. The consumer owns all state.
 */
export const useHotkeyFire = (args: UseHotkeyFireArgs): void => {
  const isDisabled = Boolean(args.isDisabled);
  const { combos, triggerCount, keysHeld, onFire } = args;

  const previousKeysHeldRef = useRef<string[]>([]);
  const comboStateRef = useRef<Map<string, { contaminated: boolean }>>(
    new Map(),
  );
  const wasDisabledRef = useRef(false);

  useEffect(() => {
    if (isDisabled) {
      previousKeysHeldRef.current = keysHeld;
      comboStateRef.current.clear();
      wasDisabledRef.current = true;
      return;
    }
    const wasDisabled = wasDisabledRef.current;
    wasDisabledRef.current = false;

    const normalize = (key: string) => key.toLowerCase();
    const toNormalizedSet = (keys: string[]) =>
      new Set(keys.map((key) => normalize(key)));
    const getComboId = (requiredKeys: Set<string>) =>
      Array.from(requiredKeys).sort().join("+");

    const previousSet = toNormalizedSet(previousKeysHeldRef.current);
    const currentSet = toNormalizedSet(keysHeld);
    const activeComboIds = new Set<string>();

    let shouldFire = false;
    for (const combo of combos) {
      if (combo.length === 0) {
        continue;
      }

      const requiredSet = toNormalizedSet(combo);
      if (requiredSet.size === 0) {
        continue;
      }

      const comboId = getComboId(requiredSet);
      activeComboIds.add(comboId);

      const comboState = comboStateRef.current.get(comboId) ?? {
        contaminated: false,
      };

      const previousIncludesAll = Array.from(requiredSet).every((key) =>
        previousSet.has(key),
      );
      const currentIncludesAll = Array.from(requiredSet).every((key) =>
        currentSet.has(key),
      );

      const previousExact =
        previousIncludesAll && previousSet.size === requiredSet.size;
      const currentExact =
        currentIncludesAll && currentSet.size === requiredSet.size;

      if (wasDisabled && currentIncludesAll) {
        comboState.contaminated = true;
      }

      if (!previousIncludesAll && currentIncludesAll) {
        comboState.contaminated = false;
      }

      if (currentIncludesAll && !currentExact) {
        comboState.contaminated = true;
      }

      if (
        previousExact &&
        !currentExact &&
        !currentIncludesAll &&
        !comboState.contaminated
      ) {
        shouldFire = true;
      }

      if (!currentIncludesAll) {
        comboState.contaminated = false;
      }

      comboStateRef.current.set(comboId, comboState);

      if (shouldFire) {
        break;
      }
    }

    for (const comboId of comboStateRef.current.keys()) {
      if (!activeComboIds.has(comboId)) {
        comboStateRef.current.delete(comboId);
      }
    }

    if (shouldFire) {
      onFire?.();
    }

    previousKeysHeldRef.current = keysHeld;
  }, [keysHeld, combos, isDisabled, onFire]);

  const prevTriggerCountRef = useRef(triggerCount);

  useEffect(() => {
    if (!isDisabled && triggerCount > prevTriggerCountRef.current) {
      onFire?.();
    }
    prevTriggerCountRef.current = triggerCount;
  }, [triggerCount, isDisabled, onFire]);
};

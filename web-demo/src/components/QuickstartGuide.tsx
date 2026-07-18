import { useMemo, useState } from "react";
import { type MessageKey, useI18n } from "../i18n";

export type GuideAction = "open-conflicts" | "open-history" | "open-row-diff";
export type GuideTrack = "basics" | "branches" | "conflicts" | "merge";

export interface QuickstartStep {
  action?: GuideAction;
  actionLabelKey?: MessageKey;
  command?: string;
  commands?: string[];
  completeOnRun?: boolean;
  descriptionKey: MessageKey;
  id: string;
  titleKey: MessageKey;
  track: GuideTrack;
}

const demoImageUrl = `${import.meta.env.BASE_URL}demo-assets/graft-app-state.png`;

export const GUIDE_TRACKS: Array<{
  descriptionKey: MessageKey;
  id: GuideTrack;
  labelKey: MessageKey;
}> = [
  {
    descriptionKey: "guide.track.basics.description",
    id: "basics",
    labelKey: "guide.track.basics",
  },
  {
    descriptionKey: "guide.track.branches.description",
    id: "branches",
    labelKey: "guide.track.branches",
  },
  {
    descriptionKey: "guide.track.merge.description",
    id: "merge",
    labelKey: "guide.track.merge",
  },
  {
    descriptionKey: "guide.track.conflicts.description",
    id: "conflicts",
    labelKey: "guide.track.conflicts",
  },
];

export const QUICKSTART_STEPS: QuickstartStep[] = [
  {
    command: "graft init",
    descriptionKey: "guide.step.init.description",
    id: "init",
    titleKey: "guide.step.init.title",
    track: "basics",
  },
  {
    command:
      "graft --db data.sqlite sql \"CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes(body) VALUES ('first note');\"",
    descriptionKey: "guide.step.seed.description",
    id: "seed-sqlite",
    titleKey: "guide.step.seed.title",
    track: "basics",
  },
  {
    command: 'echo "# Graft browser notes" > README.md',
    descriptionKey: "guide.step.file.description",
    id: "create-file",
    titleKey: "guide.step.file.title",
    track: "basics",
  },
  {
    commands: [
      `curl --create-dirs ${demoImageUrl} -o attachments/graft-app-state.png`,
      'echo "![Graft app state](attachments/graft-app-state.png)" >> README.md',
      "open README.md",
    ],
    descriptionKey: "guide.step.image.description",
    id: "embed-image",
    titleKey: "guide.step.image.title",
    track: "basics",
  },
  {
    command: "graft status",
    descriptionKey: "guide.step.status.description",
    id: "status",
    titleKey: "guide.step.status.title",
    track: "basics",
  },
  {
    command: "graft add --all",
    descriptionKey: "guide.step.stage.description",
    id: "stage",
    titleKey: "guide.step.stage.title",
    track: "basics",
  },
  {
    command: 'graft commit -m "seed app state"',
    descriptionKey: "guide.step.commit.description",
    id: "commit",
    titleKey: "guide.step.commit.title",
    track: "basics",
  },
  {
    command:
      "graft --db data.sqlite sql \"INSERT INTO notes(body) VALUES ('second note');\"",
    descriptionKey: "guide.step.change.description",
    id: "change-sqlite",
    titleKey: "guide.step.change.title",
    track: "basics",
  },
  {
    action: "open-row-diff",
    actionLabelKey: "guide.action.rowDiff",
    descriptionKey: "guide.step.diff.description",
    id: "row-diff",
    titleKey: "guide.step.diff.title",
    track: "basics",
  },
  {
    commands: ["graft add --all", 'graft commit -m "add second note"'],
    descriptionKey: "guide.step.secondCommit.description",
    id: "second-commit",
    titleKey: "guide.step.secondCommit.title",
    track: "basics",
  },
  {
    command: "graft switch -c feature/labels",
    descriptionKey: "guide.step.createBranch.description",
    id: "create-feature-branch",
    titleKey: "guide.step.createBranch.title",
    track: "branches",
  },
  {
    command:
      "graft --db data.sqlite sql \"UPDATE notes SET body = 'feature wording' WHERE id = 1;\"",
    descriptionKey: "guide.step.featureEdit.description",
    id: "feature-edit",
    titleKey: "guide.step.featureEdit.title",
    track: "branches",
  },
  {
    commands: ["graft add --all", 'graft commit -m "edit note on feature"'],
    descriptionKey: "guide.step.featureCommit.description",
    id: "feature-commit",
    titleKey: "guide.step.featureCommit.title",
    track: "branches",
  },
  {
    command: "graft switch main",
    descriptionKey: "guide.step.switchMain.description",
    id: "switch-main",
    titleKey: "guide.step.switchMain.title",
    track: "branches",
  },
  {
    command:
      "graft --db data.sqlite sql \"UPDATE notes SET body = 'main wording' WHERE id = 1;\"",
    descriptionKey: "guide.step.mainEdit.description",
    id: "main-edit",
    titleKey: "guide.step.mainEdit.title",
    track: "branches",
  },
  {
    commands: ["graft add --all", 'graft commit -m "edit note on main"'],
    descriptionKey: "guide.step.mainCommit.description",
    id: "main-commit",
    titleKey: "guide.step.mainCommit.title",
    track: "branches",
  },
  {
    command: "graft branch",
    descriptionKey: "guide.step.listBranches.description",
    id: "list-branches",
    titleKey: "guide.step.listBranches.title",
    track: "branches",
  },
  {
    command: "graft log",
    descriptionKey: "guide.step.branchGraph.description",
    id: "inspect-divergence",
    titleKey: "guide.step.branchGraph.title",
    track: "merge",
  },
  {
    command: "graft merge feature/labels",
    descriptionKey: "guide.step.merge.description",
    id: "merge-feature",
    titleKey: "guide.step.merge.title",
    track: "merge",
  },
  {
    command: "graft status",
    descriptionKey: "guide.step.mergeStatus.description",
    id: "merge-status",
    titleKey: "guide.step.mergeStatus.title",
    track: "merge",
  },
  {
    command: "graft conflicts --json",
    descriptionKey: "guide.step.inspectConflicts.description",
    id: "inspect-conflicts",
    titleKey: "guide.step.inspectConflicts.title",
    track: "conflicts",
  },
  {
    action: "open-conflicts",
    actionLabelKey: "guide.action.conflicts",
    completeOnRun: false,
    descriptionKey: "guide.step.resolve.description",
    id: "resolve-conflict",
    titleKey: "guide.step.resolve.title",
    track: "conflicts",
  },
  {
    command: 'graft merge --continue -m "merge feature labels"',
    descriptionKey: "guide.step.continue.description",
    id: "continue-merge",
    titleKey: "guide.step.continue.title",
    track: "conflicts",
  },
  {
    command: "graft status",
    descriptionKey: "guide.step.verify.description",
    id: "verify-merge",
    titleKey: "guide.step.verify.title",
    track: "conflicts",
  },
  {
    action: "open-history",
    actionLabelKey: "guide.action.history",
    descriptionKey: "guide.step.history.description",
    id: "inspect-history",
    titleKey: "guide.step.history.title",
    track: "conflicts",
  },
];

interface QuickstartGuideProps {
  completed: string[];
  onClose: () => void;
  onResetProgress: () => void;
  onRunAction: (action: GuideAction) => Promise<boolean>;
  onRunCommand: (command: string) => Promise<boolean>;
  onToggle: (id: string) => void;
}

export function QuickstartGuide({
  completed,
  onClose,
  onResetProgress,
  onRunAction,
  onRunCommand,
  onToggle,
}: QuickstartGuideProps) {
  const { t } = useI18n();
  const [running, setRunning] = useState<string>();
  const [activeTrack, setActiveTrack] = useState<GuideTrack>(() => {
    const completedSet = new Set(completed);
    return QUICKSTART_STEPS.find((step) => !completedSet.has(step.id))?.track ?? "conflicts";
  });
  const completedSet = new Set(completed);
  const percent = Math.round((completed.length / QUICKSTART_STEPS.length) * 100);
  const track = GUIDE_TRACKS.find((candidate) => candidate.id === activeTrack)!;
  const steps = useMemo(
    () => QUICKSTART_STEPS.filter((step) => step.track === activeTrack),
    [activeTrack],
  );

  async function runStep(step: QuickstartStep) {
    setRunning(step.id);
    try {
      const commands = step.commands ?? (step.command ? [step.command] : []);
      let success = step.action ? await onRunAction(step.action) : commands.length > 0;
      for (const command of commands) {
        if (!(await onRunCommand(command))) {
          success = false;
          break;
        }
      }
      if (success && step.completeOnRun !== false && !completedSet.has(step.id)) {
        onToggle(step.id);
      }
    } finally {
      setRunning(undefined);
    }
  }

  return (
    <aside className="quickstart-sidebar" aria-label={t("guide.label")}>
      <header className="guide-header">
        <div>
          <span>{t("guide.eyebrow")}</span>
          <strong>{t("guide.title")}</strong>
        </div>
        <button
          aria-label={t("guide.close")}
          onClick={onClose}
          title={t("guide.close")}
          type="button"
        >
          ×
        </button>
      </header>

      <div className="guide-progress">
        <strong>{completed.length} / {QUICKSTART_STEPS.length}</strong>
        <span>{t("guide.complete", { percent })}</span>
      </div>

      <div className="guide-intro">
        <p>{t("guide.intro")}</p>
      </div>

      <nav className="guide-tracks" aria-label={t("guide.tracks")}>
        {GUIDE_TRACKS.map((candidate, index) => {
          const candidateSteps = QUICKSTART_STEPS.filter(
            (step) => step.track === candidate.id,
          );
          const done = candidateSteps.filter((step) => completedSet.has(step.id)).length;
          return (
            <button
              aria-label={`${t(candidate.labelKey)} ${done}/${candidateSteps.length}`}
              aria-current={candidate.id === activeTrack ? "step" : undefined}
              key={candidate.id}
              onClick={() => setActiveTrack(candidate.id)}
              type="button"
            >
              <span className="guide-track-index">{index + 1}</span>
              <strong>{t(candidate.labelKey)}</strong>
              <small>{done}/{candidateSteps.length}</small>
              <span className="guide-track-fill" aria-hidden="true">
                <span style={{ width: `${(done / candidateSteps.length) * 100}%` }} />
              </span>
            </button>
          );
        })}
      </nav>

      <div className="guide-track-intro">
        <span>{t(track.labelKey)}</span>
        <p>{t(track.descriptionKey)}</p>
      </div>

      <ol className="guide-steps">
        {steps.map((step) => {
          const globalIndex = QUICKSTART_STEPS.indexOf(step);
          const done = completedSet.has(step.id);
          const title = t(step.titleKey);
          const commands = step.commands ?? (step.command ? [step.command] : []);
          return (
            <li className={done ? "is-done" : ""} key={step.id}>
              <button
                aria-label={t(done ? "guide.markUndone" : "guide.markDone", { title })}
                aria-pressed={done}
                className="guide-check"
                onClick={() => onToggle(step.id)}
                type="button"
              >
                {done ? "✓" : globalIndex + 1}
              </button>
              <div className="guide-step-copy">
                <strong>{title}</strong>
                <p>{t(step.descriptionKey)}</p>
                <button
                  className={`guide-command ${commands.length > 1 ? "is-multiline" : ""}`}
                  disabled={Boolean(running)}
                  onClick={() => void runStep(step)}
                  title={commands.join("\n") || (step.actionLabelKey ? t(step.actionLabelKey) : "")}
                  type="button"
                >
                  {commands.length > 0 ? (
                    <code>
                      {commands.map((command) => (
                        <span key={command}>{command}</span>
                      ))}
                    </code>
                  ) : (
                    <span>{step.actionLabelKey ? t(step.actionLabelKey) : t("guide.openVersion")}</span>
                  )}
                  <b>
                    {running === step.id
                      ? t("guide.running")
                      : step.action
                        ? t("guide.open")
                        : t("guide.run")}
                  </b>
                </button>
              </div>
            </li>
          );
        })}
      </ol>

      <footer className="guide-footer">
        <p>{percent === 100 ? t("guide.finished") : t("guide.saved")}</p>
        <button onClick={onResetProgress} type="button">
          {t("guide.reset")}
        </button>
      </footer>
    </aside>
  );
}

// The object pane: the path bar, then the class, the oid, the serial,
// the size, the decoded state and the outbound references of one object.

import type { LoupeObject } from "./api";
import { referenceClass } from "./api";
import { t } from "./i18n";

interface Props {
  object: LoupeObject | null;
  path: string;
  onPathChange: (path: string) => void;
  onGo: (path: string) => void;
  onSelect: (oid: string) => void;
}

export function ObjectPane({
  object,
  path,
  onPathChange,
  onGo,
  onSelect,
}: Props) {
  return (
    <section className="pane">
      <label className="path">
        <span>{t("object.path")}</span>
        <input
          data-testid="object-path"
          type="text"
          value={path}
          placeholder={t("object.pathPlaceholder")}
          spellCheck={false}
          onChange={(event) => onPathChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              onGo(path);
            }
          }}
        />
      </label>
      {object && (
        <>
          <dl className="facts">
            <dt>{t("object.class")}</dt>
            <dd data-testid="object-class">
              {object.class ?? t("object.noClass")}
            </dd>
            <dt>{t("object.oid")}</dt>
            <dd data-testid="object-oid">{object.oid}</dd>
            <dt>{t("object.serial")}</dt>
            <dd data-testid="object-serial">{object.serial}</dd>
            <dt>{t("object.size")}</dt>
            <dd>{t("object.sizeBytes", { size: object.size })}</dd>
          </dl>
          <h2>{t("object.state")}</h2>
          {object.error && (
            <p className="error">
              {t("object.refused", { error: object.error })}
            </p>
          )}
          <pre className="state" data-testid="object-state">
            {JSON.stringify(object.state, null, 2)}
          </pre>
          <h2>{t("object.references")}</h2>
          {object.references.length === 0 ? (
            <p>{t("object.noReferences")}</p>
          ) : (
            <ul className="references">
              {object.references.map((reference) => (
                <li key={`${reference.oid}@${reference.at}`}>
                  <button
                    type="button"
                    onClick={() => onSelect(reference.oid)}
                  >
                    <span className="at">{reference.at}</span>
                    <span className="class">
                      {referenceClass(object.state, reference.at) ?? ""}
                    </span>
                    <span className="oid">{reference.oid}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </section>
  );
}

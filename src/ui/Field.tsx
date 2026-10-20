import { useId, type InputHTMLAttributes } from "react";

/**
 * A one-line text field, the height and border of the select. `mono` for text that git or the
 * playbook reads literally (a path pattern, a branch, a command). `error` turns the border red and
 * says what is wrong under the field, tied to it for screen readers.
 */
export function TextField(props: InputHTMLAttributes<HTMLInputElement> & { mono?: boolean; error?: string | null }) {
  const { mono, error, className, id, ...rest } = props;
  const own = useId();
  const fieldId = id ?? own;
  const errorId = `${fieldId}-error`;
  return (
    <span className={`ui-field${className ? ` ${className}` : ""}`}>
      <input
        id={fieldId}
        className={`ui-field-input${mono ? " ui-field-mono" : ""}${error ? " ui-field-invalid" : ""}`}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? errorId : undefined}
        spellCheck={mono ? false : undefined}
        {...rest}
      />
      {error && (
        <span id={errorId} className="ui-field-error" role="alert">
          {error}
        </span>
      )}
    </span>
  );
}

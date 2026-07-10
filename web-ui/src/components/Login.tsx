import { createSignal } from "solid-js";
import { Button, Card, Input, Spinner } from "@forge/ui";
import { doLogin, state } from "../store";

export default function Login() {
  const [user, setUser] = createSignal(state.authUser ?? "");
  const [pass, setPass] = createSignal("");
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);

  const submit = async (e: Event) => {
    e.preventDefault();
    setErr("");
    setBusy(true);
    try {
      await doLogin(user(), pass());
    } catch (ex) {
      setErr(String(ex instanceof Error ? ex.message : ex));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="login-wrap">
      <Card
        class="login-card"
        title={
          <span>
            <span class="brand-vm">vm</span>
            <span class="brand-lab">lab</span> — sign in
          </span>
        }
      >
        <form class="login-form" onSubmit={submit}>
          <Input
            label="Username"
            value={user()}
            onInput={(e) => setUser(e.currentTarget.value)}
            autofocus
          />
          <Input
            label="Password"
            type="password"
            value={pass()}
            onInput={(e) => setPass(e.currentTarget.value)}
            error={!!err()}
            help={err() || undefined}
          />
          <Button variant="primary" type="submit" disabled={busy()}>
            {busy() ? <Spinner size={12} /> : null}
            {busy() ? "Signing in…" : "Sign in"}
          </Button>
        </form>
      </Card>
    </div>
  );
}

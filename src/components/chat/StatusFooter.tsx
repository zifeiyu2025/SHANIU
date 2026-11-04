import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';

export function StatusFooter({ store }: { store: SessionStore }) {
  const status = useStoreSelector(store, (s) => s.status);
  const currentTurn = useStoreSelector(store, (s) => s.currentTurn);
  const usage = useStoreSelector(store, (s) => s.usage);
  return (
    <div className="flex items-center justify-between border-t border-border-subtle px-4 py-2 text-xs text-zinc-500">
      <span data-testid="turn-status">
        {status === 'running' ? `运行中 · turn ${currentTurn}` : '空闲'}
      </span>
      {usage && (
        <span data-testid="token-usage">
          tokens ↑{usage.inputTokens} ↓{usage.outputTokens}
        </span>
      )}
    </div>
  );
}

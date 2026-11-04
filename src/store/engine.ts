import { useSyncExternalStore } from 'react';
import { produce } from 'immer';
import { createStore } from 'zustand/vanilla';
import { subscribeWithSelector } from 'zustand/middleware';

/**
 * 快照存储（ADR-006）：zustand vanilla + immer + subscribeWithSelector。
 * - update() 经 immer produce，结构共享保证选择器引用稳定；
 * - subscribe 支持两种签名：全量监听 / 选择器监听（选择切片不变则不通知）。
 */
export interface SnapshotStore<T> {
  getState(): T;
  setState(next: T): void;
  subscribe(listener: (state: T, prevState: T) => void): () => void;
  subscribe<S>(selector: (state: T) => S, listener: (slice: S, prevSlice: S) => void): () => void;
  update(recipe: (draft: T) => void): void;
}

export function createSnapshotStore<T>(initial: T): SnapshotStore<T> {
  const store = createStore<T>()(
    subscribeWithSelector(() => initial),
  );
  return {
    getState: store.getState,
    setState: (next) => store.setState(next),
    subscribe: store.subscribe,
    update: (recipe) => {
      store.setState(produce(store.getState(), recipe));
    },
  };
}

/**
 * React 订阅入口。注意：selector 必须返回结构共享的稳定引用（state 的字段 / immer 产出的数组），
 * 不得在 selector 内新建对象/数组，否则 useSyncExternalStore 会因快照引用漂移而无限重渲染。
 */
export function useStoreSelector<T, S>(store: SnapshotStore<T>, selector: (state: T) => S): S {
  return useSyncExternalStore(store.subscribe, () => selector(store.getState()));
}

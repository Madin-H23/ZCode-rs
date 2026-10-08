interface RootStartupGateState {
  isResolvingStartupAuthState: boolean;
  isResolvingProviderStartupState: boolean;
  isRestoring: boolean;
  isBootstrappingInitialWorkspace: boolean;
}

interface RootStartupLoadingVisibilityState extends RootStartupGateState {
  isDesktop: boolean | undefined;
  welcomeScreenOpen: boolean;
}

interface FallbackWorkspaceCreateState {
  isMounted: boolean;
  activeWorkspacePath: string | null;
}

interface ProviderStartupSyncState {
  providerFamilyDomainMigrationComplete: boolean;
  modelSelectionViewHydrated: boolean;
  /** 设置快照已加载或读取失败；门禁的 providerFamilyDomain 来自它。 */
  appSettingsHydrated: boolean;
}

interface ProviderStartupResolutionState {
  providerStartupSyncPending: boolean;
  providerAvailabilityStartupCheckCompleted: boolean;
}

export function shouldBlockRootRender(state: RootStartupGateState): boolean {
  return (
    state.isResolvingStartupAuthState ||
    state.isResolvingProviderStartupState ||
    state.isRestoring ||
    state.isBootstrappingInitialWorkspace
  );
}

export function shouldShowRootStartupLoading(state: RootStartupLoadingVisibilityState): boolean {
  // 登录入口是启动门禁的结果，不是可继续被门禁遮挡的后台状态。
  // 如果 WelcomeScreen 已经打开，继续返回启动 loading 会把未登录用户卡在黑屏 logo。
  return Boolean(state.isDesktop) && !state.welcomeScreenOpen && shouldBlockRootRender(state);
}

export function shouldEnableProviderAvailabilityLoginEntryGuard(): boolean {
  return true;
}

export function shouldResolveProviderStartupState(state: ProviderStartupResolutionState): boolean {
  return state.providerStartupSyncPending || !state.providerAvailabilityStartupCheckCompleted;
}

export function shouldOpenFallbackWorkspaceAfterCreate(
  state: FallbackWorkspaceCreateState,
): boolean {
  return state.isMounted && !state.activeWorkspacePath;
}

export function isProviderStartupSyncPending(state: ProviderStartupSyncState): boolean {
  // Bugfix：provider 启动门禁只检查一次，判据里的 providerFamilyDomain 来自设置快照。迁移流程
  // 自己读一次设置后就会结束等待，而 useSettings 的那次读取可能更晚返回（Host 并发应答、顺序不保证），
  // reload 后门禁会用 null 的设置误开登录页并停在那里。设置快照就绪前保持 pending。
  return (
    !state.providerFamilyDomainMigrationComplete ||
    !state.modelSelectionViewHydrated ||
    !state.appSettingsHydrated
  );
}

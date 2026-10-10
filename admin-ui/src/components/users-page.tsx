import { useEffect, useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  ArrowRightLeftIcon,
  BanIcon,
  CircleCheckIcon,
  EllipsisIcon,
  KeyRoundIcon,
  PlusIcon,
  Trash2Icon,
  UsersIcon,
} from 'lucide-react'
import {
  createUser,
  deleteUser,
  listUsers,
  resetUserPassword,
  setUserDisabled,
  setUserParent,
  type ConsoleUser,
  type CreateUserInput,
} from '@/api/users'
import { useI18n } from '@/lib/i18n'
import { useMe } from '@/lib/role'
import { cn, extractError, formatFullTime } from '@/lib/utils'
import { useDocumentTitle } from '@/lib/use-document-title'
import { AppFooter } from '@/components/app-footer'
import { AccountMenu, AppHeader, MainNav, type MainSection } from '@/components/app-header'
import { CopyButton } from '@/components/copy-button'
import { MIN_PASSWORD_LENGTH, PasswordInput } from '@/components/password-input'
import { ErrorState } from '@/components/state-placeholders'
import {
  AlertDialog,
  AlertDialogClose,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogPopup,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Badge } from '@/components/ui/badge'
import { Card } from '@/components/ui/card'
import { Button, buttonVariants } from '@/components/ui/button'
import {
  Dialog,
  DialogClose,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Field, FieldDescription, FieldError, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Menu, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from '@/components/ui/menu'
import { Radio, RadioGroup } from '@/components/ui/radio-group'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'

/** 弹框里正在处理哪一个账号、做什么。 */
type Pending =
  | { kind: 'password'; user: ConsoleUser }
  | { kind: 'move'; user: ConsoleUser }
  | { kind: 'disable'; user: ConsoleUser }
  | { kind: 'delete'; user: ConsoleUser }

/**
 * 成员管理：管理员管全部代理和用户，代理只管自己的下属用户。
 *
 * 代理看不到下属的号，这里也就不出现号数；停用代理会连带停用它名下的用户（登录不了、名下的号
 * 不接流量）。删除前要先清空：代理名下还有用户、或账号名下还有号时后端拒绝。
 */
export function UsersPage({
  onNavigate,
  onSignOut,
}: {
  onNavigate: (section: MainSection) => void
  onSignOut: () => void
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const me = useMe().data
  const isAdmin = me?.role === 'admin'
  const [creating, setCreating] = useState(false)
  const [pending, setPending] = useState<Pending | null>(null)
  const usersQuery = useQuery({ queryKey: ['users'], queryFn: listUsers })
  const users = useMemo(() => usersQuery.data ?? [], [usersQuery.data])
  const agents = useMemo(() => users.filter((u) => u.role === 'agent'), [users])

  useDocumentTitle(`${t('成员管理', 'Members')} · Luban`)

  const done = (title: string) => {
    void qc.invalidateQueries({ queryKey: ['users'] })
    setPending(null)
    toastManager.add({ title, type: 'success' })
  }
  const failed = (error: unknown) => {
    toastManager.add({
      title: t('操作失败', 'Operation failed'),
      description: extractError(error, language),
      type: 'error',
    })
  }
  const toggleDisabled = useMutation({
    mutationFn: ({ user, disabled }: { user: ConsoleUser; disabled: boolean }) =>
      setUserDisabled(user.id, disabled),
    onSuccess: (_r, { disabled }) => done(disabled ? t('成员已停用', 'Member disabled') : t('成员已启用', 'Member enabled')),
    onError: failed,
  })
  const remove = useMutation({
    mutationFn: (user: ConsoleUser) => deleteUser(user.id),
    onSuccess: () => done(t('成员已删除', 'Member deleted')),
    onError: failed,
  })

  const roleLabel = (role: ConsoleUser['role']) => (role === 'agent' ? t('代理', 'Agent') : t('用户', 'User'))
  const statusBadge = (user: ConsoleUser) => {
    if (user.disabled) return <Badge variant="error">{t('已停用', 'Disabled')}</Badge>
    if (user.parent_disabled) {
      return <Badge variant="warning">{t('随上级停用', 'Disabled with parent')}</Badge>
    }
    return <Badge variant="success">{t('正常', 'Active')}</Badge>
  }

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader
        actions={<AccountMenu onNavigate={onNavigate} onSignOut={onSignOut} />}
        nav={<MainNav current="users" onNavigate={onNavigate} />}
        onNavigateHome={() => onNavigate('pool')}
      />

      <main className="page-frame relative flex-1 py-4 pb-8 sm:py-5 sm:pb-10">
        <div className="space-y-3 sm:space-y-4">
          {/* 页头卡片：与账号池、费用同构——标题与计数在左，主动作在右。 */}
          <section aria-labelledby="users-page-title" className="overflow-hidden rounded-2xl border bg-card shadow-xs/5">
            <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-4 sm:px-5">
              <div className="flex min-w-0 flex-wrap items-center gap-2.5">
                <h1 className="min-w-0 text-lg font-semibold tracking-tight" id="users-page-title">
                  {t('成员管理', 'Members')}
                </h1>
                {!usersQuery.isPending && (
                  <Hint
                    label={isAdmin
                      ? t(
                          '为代理与用户创建控制台登录。账号归添加者所有，代理看不到下属用户的账号。',
                          'Create console sign-ins for agents and users. Accounts belong to the member who added them; agents cannot see their users’ accounts.',
                        )
                      : t('为你创建下属用户。下属用户添加的账号对你不可见。', 'Create users under you. Accounts they add are not visible to you.')}
                  >
                    <Badge variant="secondary">
                      {isAdmin
                        ? t(`${agents.length} 个代理 · ${users.length - agents.length} 个用户`, `${agents.length} agents · ${users.length - agents.length} users`)
                        : t(`${users.length} 个用户`, `${users.length} users`)}
                    </Badge>
                  </Hint>
                )}
              </div>
              <Button size="sm" onClick={() => setCreating(true)}>
                <PlusIcon />
                {isAdmin ? t('新建成员', 'New member') : t('新建用户', 'New user')}
              </Button>
            </div>
          </section>

          {usersQuery.isPending ? (
            <Card className="space-y-2 p-4">
              {Array.from({ length: 4 }, (_, i) => <Skeleton className="h-9 w-full" key={i} />)}
            </Card>
          ) : usersQuery.isError ? (
            <Card>
              <ErrorState
                error={usersQuery.error}
                retrying={usersQuery.isFetching}
                onRetry={() => void usersQuery.refetch()}
              />
            </Card>
          ) : users.length === 0 ? (
            <Card>
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon"><UsersIcon /></EmptyMedia>
                  <EmptyTitle>{t('暂无成员', 'No members yet')}</EmptyTitle>
                  <EmptyDescription>
                    {isAdmin
                      ? t('创建代理或用户后，对方即可登录控制台并添加账号。', 'Once created, agents and users can sign in and add accounts.')
                      : t('创建用户后，对方即可登录控制台并添加账号。', 'Once created, users can sign in and add accounts.')}
                  </EmptyDescription>
                </EmptyHeader>
              </Empty>
            </Card>
          ) : (
              <Table variant="card">
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('用户名', 'Username')}</TableHead>
                    {isAdmin && <TableHead>{t('角色', 'Role')}</TableHead>}
                    {isAdmin && <TableHead>{t('上级', 'Parent')}</TableHead>}
                    {isAdmin && <TableHead className="text-right">{t('账号数', 'Accounts')}</TableHead>}
                    {isAdmin && <TableHead className="text-right">{t('下属用户', 'Users')}</TableHead>}
                    <TableHead>{t('状态', 'Status')}</TableHead>
                    <TableHead className="whitespace-nowrap">{t('创建时间', 'Created')}</TableHead>
                    <TableHead className="w-10"><span className="sr-only">{t('操作', 'Actions')}</span></TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {users.map((user) => (
                    <TableRow key={user.id}>
                      <TableCell className="font-medium">{user.username}</TableCell>
                      {isAdmin && (
                        <TableCell>
                          <Badge variant={user.role === 'agent' ? 'info' : 'secondary'}>{roleLabel(user.role)}</Badge>
                        </TableCell>
                      )}
                      {isAdmin && <TableCell className="text-muted-foreground">{user.parent_username ?? '—'}</TableCell>}
                      {isAdmin && <TableCell className="text-right tabular-nums">{user.credential_count ?? 0}</TableCell>}
                      {isAdmin && (
                        <TableCell className="text-right tabular-nums">
                          {user.role === 'agent' ? user.child_count : '—'}
                        </TableCell>
                      )}
                      <TableCell>{statusBadge(user)}</TableCell>
                      <TableCell className="whitespace-nowrap text-muted-foreground tabular-nums">
                        {formatFullTime(user.created_at, language)}
                      </TableCell>
                      <TableCell>
                        <Menu>
                          <MenuTrigger
                            aria-label={t(`${user.username} 的操作`, `Actions for ${user.username}`)}
                            className={buttonVariants({ size: 'icon-sm', variant: 'ghost' })}
                          >
                            <EllipsisIcon />
                          </MenuTrigger>
                          <MenuPopup align="end" className="w-44">
                            <MenuItem onClick={() => setPending({ kind: 'password', user })}>
                              <KeyRoundIcon />{t('重置密码', 'Reset password')}
                            </MenuItem>
                            {user.disabled ? (
                              <MenuItem onClick={() => toggleDisabled.mutate({ user, disabled: false })}>
                                <CircleCheckIcon />{t('启用', 'Enable')}
                              </MenuItem>
                            ) : (
                              <MenuItem onClick={() => setPending({ kind: 'disable', user })}>
                                <BanIcon />{t('停用', 'Disable')}
                              </MenuItem>
                            )}
                            {isAdmin && user.role === 'user' && (
                              <MenuItem onClick={() => setPending({ kind: 'move', user })}>
                                <ArrowRightLeftIcon />{t('转移上级', 'Change parent')}
                              </MenuItem>
                            )}
                            <MenuSeparator />
                            <MenuItem variant="destructive" onClick={() => setPending({ kind: 'delete', user })}>
                              <Trash2Icon />{t('删除', 'Delete')}
                            </MenuItem>
                          </MenuPopup>
                        </Menu>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
          )}
        </div>
      </main>
      <AppFooter />

      <CreateUserDialog
        agents={agents}
        isAdmin={isAdmin}
        open={creating}
        onOpenChange={setCreating}
        onCreated={() => { void qc.invalidateQueries({ queryKey: ['users'] }) }}
      />
      <ResetPasswordDialog
        user={pending?.kind === 'password' ? pending.user : null}
        onClose={() => setPending(null)}
      />
      <MoveUserDialog
        adminName={me?.username ?? 'admin'}
        adminId={me?.id ?? 0}
        agents={agents}
        user={pending?.kind === 'move' ? pending.user : null}
        onClose={() => setPending(null)}
        onDone={() => done(t('已转移', 'Moved'))}
      />

      <AlertDialog
        open={pending?.kind === 'disable'}
        onOpenChange={(next) => { if (!next && !toggleDisabled.isPending) setPending(null) }}
      >
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('停用成员', 'Disable member')}</AlertDialogTitle>
            <AlertDialogDescription>
              {pending?.kind === 'disable' && (pending.user.role === 'agent'
                ? t(
                    `停用代理 ${pending.user.username} 后，该代理及其全部下属用户均无法登录，其账号也不再承接流量。`,
                    `Once agent ${pending.user.username} is disabled, neither the agent nor any of its users can sign in, and their accounts stop serving traffic.`,
                  )
                : t(
                    `停用 ${pending.user.username} 后，该成员无法登录，其账号不再承接流量。`,
                    `Once ${pending.user.username} is disabled, this member can no longer sign in, and their accounts stop serving traffic.`,
                  ))}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={toggleDisabled.isPending} variant="ghost" />}>
              {t('取消', 'Cancel')}
            </AlertDialogClose>
            <Button
              loading={toggleDisabled.isPending}
              variant="destructive"
              onClick={() => {
                if (pending?.kind === 'disable') toggleDisabled.mutate({ user: pending.user, disabled: true })
              }}
            >
              {t('停用', 'Disable')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>

      <AlertDialog
        open={pending?.kind === 'delete'}
        onOpenChange={(next) => { if (!next && !remove.isPending) setPending(null) }}
      >
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('删除成员', 'Delete member')}</AlertDialogTitle>
            <AlertDialogDescription>
              {pending?.kind === 'delete' && t(
                `删除 ${pending.user.username} 后无法恢复。该成员仍有账号（或代理仍有下属用户）时无法删除，请先移除。`,
                `Deleting ${pending.user.username} cannot be undone. A member who still owns accounts (or an agent who still has users) cannot be deleted; remove them first.`,
              )}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={remove.isPending} variant="ghost" />}>
              {t('取消', 'Cancel')}
            </AlertDialogClose>
            <Button
              loading={remove.isPending}
              variant="destructive"
              onClick={() => { if (pending?.kind === 'delete') remove.mutate(pending.user) }}
            >
              {t('删除', 'Delete')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </div>
  )
}

/**
 * 新建代理或用户。管理员先选角色（两张卡片，说清各自能做什么），用户再选挂在谁名下；代理开的
 * 只能是挂在自己名下的用户，不出现这两项。
 *
 * 初始密码可一键生成；建好之后弹框不关，换成一张「登录信息」卡片——密码只在这一刻看得到，
 * 管理员得把它交给对方，所以给出复制按钮，而不是一句提示就关掉。
 */
function CreateUserDialog({
  agents,
  isAdmin,
  open,
  onOpenChange,
  onCreated,
}: {
  agents: ConsoleUser[]
  isAdmin: boolean
  open: boolean
  onOpenChange: (open: boolean) => void
  onCreated: (user: ConsoleUser) => void
}) {
  const { t, language } = useI18n()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [role, setRole] = useState<'agent' | 'user'>('user')
  // 'self' = 挂在管理员自己名下，否则是代理的 id。
  const [parent, setParent] = useState<string>('self')
  const [created, setCreated] = useState<{ user: ConsoleUser; password: string } | null>(null)

  useEffect(() => {
    if (!open) return
    setUsername('')
    setPassword('')
    setRole('user')
    setParent('self')
    setCreated(null)
  }, [open])

  // 提交的内容整份作为变量传进去，成功页展示的是**提交时**的密码：等待响应期间输入框
  // 被改了，读当前状态会显示并复制一份服务器没存的密码。
  const create = useMutation({
    mutationFn: (input: CreateUserInput) => createUser(input),
    onSuccess: (user, input) => {
      setCreated({ user, password: input.password })
      onCreated(user)
    },
  })
  const submit = () => create.mutate({
    username: username.trim(),
    password: password.trim(),
    ...(isAdmin ? { role } : {}),
    ...(isAdmin && role === 'user' && parent !== 'self' ? { parent_id: Number(parent) } : {}),
  })

  const roleOptions = [
    {
      value: 'user' as const,
      title: t('用户', 'User'),
      description: t('添加账号，并查看自己账号的用量。', 'Adds accounts and views the usage of their own accounts.'),
    },
    {
      value: 'agent' as const,
      title: t('代理', 'Agent'),
      description: t('添加账号，并可创建下属用户。', 'Adds accounts and can create their own users.'),
    },
  ]
  const parentItems = [
    { value: 'self', label: t('管理员直属', 'Admin (direct)') },
    ...agents.map((a) => ({ value: String(a.id), label: a.username })),
  ]
  const usernameTooShort = username.trim().length > 0 && username.trim().length < 2
  const passwordTooShort = password.trim().length > 0 && password.trim().length < MIN_PASSWORD_LENGTH
  const canSubmit = username.trim().length >= 2 && password.trim().length >= MIN_PASSWORD_LENGTH
  const kind = isAdmin && role === 'agent' ? t('代理', 'agent') : t('用户', 'user')

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!create.isPending) onOpenChange(next) }}>
      <DialogPopup>
        {created ? (
          <>
            <DialogHeader>
              <DialogTitle>{t('成员已创建', 'Member created')}</DialogTitle>
              <DialogDescription>
                {t(
                  '请将以下登录信息交给对方。关闭后将无法再查看密码，忘记时只能重置。',
                  'Pass the following sign-in details on to the new member. The password cannot be shown again after closing; if forgotten, it can only be reset.',
                )}
              </DialogDescription>
            </DialogHeader>
            <DialogPanel>
              <SignInDetails password={created.password} username={created.user.username} />
            </DialogPanel>
            <DialogFooter>
              <CopyButton text={signInText(created.user.username, created.password, t)}>{t('复制全部', 'Copy all')}</CopyButton>
              <DialogClose render={<Button />}>{t('完成', 'Done')}</DialogClose>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>{isAdmin ? t('新建成员', 'New member') : t('新建用户', 'New user')}</DialogTitle>
              <DialogDescription>
                {isAdmin
                  ? t('对方使用此处设置的用户名和密码登录控制台。', 'They sign in to the console with this username and password.')
                  : t('新用户归属于你，使用此处设置的用户名和密码登录控制台。', 'The new user belongs to you and signs in with this username and password.')}
              </DialogDescription>
            </DialogHeader>
            <Form
              className="contents"
              onSubmit={(event) => { event.preventDefault(); if (canSubmit && !create.isPending) submit() }}
            >
              <DialogPanel className="space-y-5">
                {isAdmin && (
                  <Field>
                    <FieldLabel>{t('角色', 'Role')}</FieldLabel>
                    {/* 选中态挂在整张卡片上（has-data-checked），点卡片任意处都能选，方向键切换由 RadioGroup 负责。 */}
                    <RadioGroup
                      aria-label={t('角色', 'Role')}
                      className="grid w-full gap-2 sm:grid-cols-2"
                      value={role}
                      onValueChange={(value) => setRole(value as 'agent' | 'user')}
                    >
                      {roleOptions.map((option) => (
                        <Label
                          className="cursor-pointer items-start gap-2.5 rounded-lg border px-3 py-2.5 transition-colors hover:bg-accent/60 has-data-checked:border-primary has-data-checked:bg-primary/6"
                          key={option.value}
                        >
                          <Radio className="mt-px" value={option.value} />
                          <span className="flex flex-col gap-1">
                            <span className="text-sm font-medium">{option.title}</span>
                            <span className="text-xs font-normal text-muted-foreground">{option.description}</span>
                          </span>
                        </Label>
                      ))}
                    </RadioGroup>
                  </Field>
                )}
                {isAdmin && role === 'user' && (
                  <Field>
                    <FieldLabel>{t('所属上级', 'Belongs to')}</FieldLabel>
                    <Select items={parentItems} value={parent} onValueChange={(v) => { if (v) setParent(v as string) }}>
                      <SelectTrigger aria-label={t('所属上级', 'Belongs to')}><SelectValue /></SelectTrigger>
                      <SelectPopup>
                        {parentItems.map((item) => <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>)}
                      </SelectPopup>
                    </Select>
                    <FieldDescription>
                      {t('归属于代理时，由该代理管理此用户。', 'When assigned to an agent, that agent manages this user.')}
                    </FieldDescription>
                  </Field>
                )}
                <Field invalid={usernameTooShort}>
                  <FieldLabel>{t('用户名', 'Username')}</FieldLabel>
                  <Input
                    autoCapitalize="none"
                    autoComplete="off"
                    autoFocus
                    onChange={(event) => setUsername(event.target.value)}
                    placeholder={t('例如 zhangsan', 'e.g. alice')}
                    spellCheck={false}
                    value={username}
                  />
                  <FieldDescription>
                    {t('2～32 个字符，可用字母、数字和 _ - . @。', '2 to 32 characters: letters, digits and _ - . @.')}
                  </FieldDescription>
                </Field>
                <Field invalid={passwordTooShort || create.isError}>
                  <FieldLabel>{t('初始密码', 'Initial password')}</FieldLabel>
                  <PasswordInput generate invalid={passwordTooShort} value={password} onChange={setPassword} />
                  <FieldDescription>
                    {t(
                      `至少 ${MIN_PASSWORD_LENGTH} 个字符，对方登录后可自行修改。`,
                      `At least ${MIN_PASSWORD_LENGTH} characters; they can change it after signing in.`,
                    )}
                  </FieldDescription>
                  {create.isError && <FieldError match>{extractError(create.error, language)}</FieldError>}
                </Field>
              </DialogPanel>
              <DialogFooter>
                <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
                <Button disabled={!canSubmit} loading={create.isPending} type="submit">
                  <PlusIcon />
                  {t(`新建${kind}`, `Create ${kind}`)}
                </Button>
              </DialogFooter>
            </Form>
          </>
        )}
      </DialogPopup>
    </Dialog>
  )
}

/** 登录信息的三行：地址、用户名、密码。新建与重置密码成功后都用它。 */
function signInRows(username: string, password: string, t: (zh: string, en: string) => string) {
  return [
    { term: t('登录地址', 'Sign-in URL'), value: `${window.location.origin}${window.location.pathname}`, mono: false },
    { term: t('用户名', 'Username'), value: username, mono: true },
    { term: t('密码', 'Password'), value: password, mono: true },
  ]
}

function signInText(username: string, password: string, t: (zh: string, en: string) => string) {
  return signInRows(username, password, t).map((row) => `${row.term}: ${row.value}`).join('\n')
}

/** 登录信息卡片：每行带一枚复制按钮。 */
function SignInDetails({ username, password }: { username: string; password: string }) {
  const { t } = useI18n()
  return (
    <dl className="divide-y rounded-lg border text-sm">
      {signInRows(username, password, t).map((row) => (
        <div className="flex items-center gap-3 px-3 py-2" key={row.term}>
          <dt className="w-16 shrink-0 text-muted-foreground">{row.term}</dt>
          <dd className={cn('min-w-0 flex-1 truncate', row.mono && 'font-mono')}>{row.value}</dd>
          <CopyButton label={t(`复制${row.term}`, `Copy ${row.term.toLowerCase()}`)} text={row.value} />
        </div>
      ))}
    </dl>
  )
}

/**
 * 重置某个账号的密码：可一键随机生成；成功后它的全部登录下线，弹框换成新的登录信息，
 * 方便复制给对方（密码只在这一刻看得到）。
 */
function ResetPasswordDialog({
  user,
  onClose,
}: {
  user: ConsoleUser | null
  onClose: () => void
}) {
  const { t, language } = useI18n()
  const [password, setPassword] = useState('')
  const [reset, setReset] = useState<string | null>(null)
  useEffect(() => {
    if (!user) return
    setPassword('')
    setReset(null)
  }, [user])
  // 同新建：成功页用提交时的密码快照，不读输入框的当前值。
  const save = useMutation({
    mutationFn: ({ id, pw }: { id: number; pw: string }) => resetUserPassword(id, pw),
    onSuccess: (_result, { pw }) => setReset(pw),
  })
  const tooShort = password.trim().length > 0 && password.trim().length < MIN_PASSWORD_LENGTH
  const canSubmit = password.trim().length >= MIN_PASSWORD_LENGTH

  return (
    <Dialog open={!!user} onOpenChange={(next) => { if (!next && !save.isPending) onClose() }}>
      <DialogPopup>
        {reset != null && user ? (
          <>
            <DialogHeader>
              <DialogTitle>{t('密码已重置', 'Password reset')}</DialogTitle>
              <DialogDescription>
                {t(
                  '该成员已有的登录已全部退出。请将新的登录信息交给对方，关闭后将无法再查看密码。',
                  'All existing sign-ins of this member were signed out. Pass the new sign-in details on; the password cannot be shown again after closing.',
                )}
              </DialogDescription>
            </DialogHeader>
            <DialogPanel>
              <SignInDetails password={reset} username={user.username} />
            </DialogPanel>
            <DialogFooter>
              <CopyButton text={signInText(user.username, reset, t)}>{t('复制全部', 'Copy all')}</CopyButton>
              <DialogClose render={<Button />}>{t('完成', 'Done')}</DialogClose>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>{t('重置密码', 'Reset password')}</DialogTitle>
              <DialogDescription>{user?.username}</DialogDescription>
            </DialogHeader>
            <Form
              className="contents"
              onSubmit={(event) => {
                event.preventDefault()
                if (canSubmit && user && !save.isPending) save.mutate({ id: user.id, pw: password.trim() })
              }}
            >
              <DialogPanel>
                <Field invalid={tooShort || save.isError}>
                  <FieldLabel>{t('新密码', 'New password')}</FieldLabel>
                  <PasswordInput autoFocus generate invalid={tooShort} value={password} onChange={setPassword} />
                  <FieldDescription>
                    {t(
                      `至少 ${MIN_PASSWORD_LENGTH} 个字符。重置后，该成员已有的登录将全部退出。`,
                      `At least ${MIN_PASSWORD_LENGTH} characters. All existing sign-ins of this member are signed out.`,
                    )}
                  </FieldDescription>
                  {save.isError && <FieldError match>{extractError(save.error, language)}</FieldError>}
                </Field>
              </DialogPanel>
              <DialogFooter>
                <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
                <Button disabled={!canSubmit} loading={save.isPending} type="submit">
                  <KeyRoundIcon />
                  {t('重置', 'Reset')}
                </Button>
              </DialogFooter>
            </Form>
          </>
        )}
      </DialogPopup>
    </Dialog>
  )
}

/** 把用户转到另一个代理（或管理员）名下，仅管理员。删除代理前用它把名下用户转走。 */
function MoveUserDialog({
  adminId,
  adminName,
  agents,
  user,
  onClose,
  onDone,
}: {
  adminId: number
  adminName: string
  agents: ConsoleUser[]
  user: ConsoleUser | null
  onClose: () => void
  onDone: () => void
}) {
  const { t, language } = useI18n()
  const [parent, setParent] = useState('')
  useEffect(() => { if (user) setParent(String(user.parent_id ?? adminId)) }, [user, adminId])
  const save = useMutation({
    mutationFn: () => setUserParent(user!.id, Number(parent)),
    onSuccess: onDone,
  })
  const items = [
    { value: String(adminId), label: t(`${adminName}（管理员直属）`, `${adminName} (admin, direct)`) },
    ...agents.map((a) => ({ value: String(a.id), label: a.username })),
  ]
  const dirty = !!user && parent !== String(user.parent_id ?? adminId)

  return (
    <Dialog open={!!user} onOpenChange={(next) => { if (!next && !save.isPending) onClose() }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('转移上级', 'Change parent')}</DialogTitle>
          <DialogDescription>{user?.username}</DialogDescription>
        </DialogHeader>
        <DialogPanel>
          <Field invalid={save.isError}>
            <FieldLabel>{t('新上级', 'New parent')}</FieldLabel>
            <Select items={items} value={parent} onValueChange={(v) => { if (v) setParent(v as string) }}>
              <SelectTrigger aria-label={t('新上级', 'New parent')}><SelectValue /></SelectTrigger>
              <SelectPopup>
                {items.map((item) => <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>)}
              </SelectPopup>
            </Select>
            <FieldDescription>
              {t('转移后由新上级管理此用户，账号仍归用户本人所有。', 'After the transfer, the new parent manages this user; the user keeps ownership of their accounts.')}
            </FieldDescription>
            {save.isError && <FieldError match>{extractError(save.error, language)}</FieldError>}
          </Field>
        </DialogPanel>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button disabled={!dirty} loading={save.isPending} onClick={() => save.mutate()}>{t('转移', 'Move')}</Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}

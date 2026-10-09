import { useEffect, useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  ArrowRightLeftIcon,
  BanIcon,
  CheckIcon,
  CircleCheckIcon,
  CopyIcon,
  DicesIcon,
  EllipsisIcon,
  EyeIcon,
  EyeOffIcon,
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
import { cn, copyText, extractError, formatFullTime } from '@/lib/utils'
import { AppFooter } from '@/components/app-footer'
import { AppHeader, MainNav, PreferencesMenu } from '@/components/app-header'
import { ChangePasswordDialog } from '@/components/change-password-dialog'
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
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import { Menu, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from '@/components/ui/menu'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'

const MIN_PASSWORD_LENGTH = 4

/** 弹框里正在处理哪一个账号、做什么。 */
type Pending =
  | { kind: 'password'; user: ConsoleUser }
  | { kind: 'move'; user: ConsoleUser }
  | { kind: 'disable'; user: ConsoleUser }
  | { kind: 'delete'; user: ConsoleUser }

/**
 * 用户管理：管理员管全部代理和用户，代理只管挂在自己名下的用户。
 *
 * 代理看不到下属的号，这里也就不出现号数；停用代理会连带停用它名下的用户（登录不了、名下的号
 * 不接流量）。删除前要先清空：代理名下还有用户、或账号名下还有号时后端拒绝。
 */
export function UsersPage({ onBack, onSignOut }: { onBack: () => void; onSignOut: () => void }) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const me = useMe().data
  const isAdmin = me?.role === 'admin'
  const [creating, setCreating] = useState(false)
  const [passwordOpen, setPasswordOpen] = useState(false)
  const [pending, setPending] = useState<Pending | null>(null)
  const usersQuery = useQuery({ queryKey: ['users'], queryFn: listUsers })
  const users = useMemo(() => usersQuery.data ?? [], [usersQuery.data])
  const agents = useMemo(() => users.filter((u) => u.role === 'agent'), [users])

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${t('用户管理', 'Users')} · Luban`
    return () => {
      document.title = previousTitle
    }
  }, [t])

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
    onSuccess: (_r, { disabled }) => done(disabled ? t('账号已停用', 'Account disabled') : t('账号已启用', 'Account enabled')),
    onError: failed,
  })
  const remove = useMutation({
    mutationFn: (user: ConsoleUser) => deleteUser(user.id),
    onSuccess: () => done(t('账号已删除', 'Account deleted')),
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
        actions={
          <PreferencesMenu onSignOut={onSignOut}>
            {/* 管理员的密码在系统设置里改；代理在这里改自己的。 */}
            {me?.role === 'agent' && (
              <MenuItem onClick={() => setPasswordOpen(true)}>
                <KeyRoundIcon />{t('修改密码', 'Change password')}
              </MenuItem>
            )}
          </PreferencesMenu>
        }
        nav={<MainNav current="users" onNavigate={(section) => { if (section === 'pool') onBack() }} />}
        onNavigateHome={onBack}
      />
      <ChangePasswordDialog open={passwordOpen} onOpenChange={setPasswordOpen} />

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        <div className="space-y-5 sm:space-y-7">
          <section aria-labelledby="users-page-title" className="flex flex-wrap items-end justify-between gap-3">
            <div className="max-w-2xl">
              <h1 className="text-xl font-semibold tracking-tight sm:text-2xl" id="users-page-title">
                {t('用户管理', 'Users')}
              </h1>
              <p className="mt-1.5 text-sm leading-6 text-muted-foreground max-sm:sr-only">
                {isAdmin
                  ? t(
                      '开设代理与用户的登录账号。每个人上的号挂在自己名下，代理看不到下属用户的号。',
                      'Create sign-ins for agents and users. Accounts belong to whoever added them; agents cannot see their users’ accounts.',
                    )
                  : t(
                      '开设挂在你名下的用户。你看不到他们上的号。',
                      'Create users under you. You cannot see the accounts they add.',
                    )}
              </p>
            </div>
            <Button size="sm" onClick={() => setCreating(true)}>
              <PlusIcon />
              {isAdmin ? t('新建账号', 'New account') : t('新建用户', 'New user')}
            </Button>
          </section>

          {usersQuery.isPending ? (
            <div className="space-y-2">
              {Array.from({ length: 4 }, (_, i) => <Skeleton className="h-10 w-full" key={i} />)}
            </div>
          ) : usersQuery.isError ? (
            <p className="text-sm text-destructive-foreground">{extractError(usersQuery.error, language)}</p>
          ) : users.length === 0 ? (
            <Empty>
              <EmptyHeader>
                <EmptyMedia variant="icon"><UsersIcon /></EmptyMedia>
                <EmptyTitle>{t('还没有账号', 'No accounts yet')}</EmptyTitle>
                <EmptyDescription>
                  {isAdmin
                    ? t('新建代理或用户后，他们即可登录控制台上号。', 'Once created, agents and users can sign in and add accounts.')
                    : t('新建用户后，他们即可登录控制台上号。', 'Once created, users can sign in and add accounts.')}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <div className="overflow-x-auto rounded-xl border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('用户名', 'Username')}</TableHead>
                    {isAdmin && <TableHead>{t('角色', 'Role')}</TableHead>}
                    {isAdmin && <TableHead>{t('上级', 'Parent')}</TableHead>}
                    {isAdmin && <TableHead className="text-right">{t('名下号数', 'Accounts')}</TableHead>}
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
            </div>
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
            <AlertDialogTitle>{t('停用账号', 'Disable account')}</AlertDialogTitle>
            <AlertDialogDescription>
              {pending?.kind === 'disable' && (pending.user.role === 'agent'
                ? t(
                    `停用代理 ${pending.user.username} 后，它和名下的全部用户都无法登录，名下的号也不再接流量。`,
                    `Disabling agent ${pending.user.username} signs it and all its users out, and their accounts stop serving traffic.`,
                  )
                : t(
                    `停用 ${pending.user.username} 后，该账号无法登录，名下的号不再接流量。`,
                    `Disabling ${pending.user.username} signs it out, and its accounts stop serving traffic.`,
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
            <AlertDialogTitle>{t('删除账号', 'Delete account')}</AlertDialogTitle>
            <AlertDialogDescription>
              {pending?.kind === 'delete' && t(
                `删除 ${pending.user.username} 后无法恢复。名下还有号（代理名下还有用户）时无法删除，须先清空。`,
                `Deleting ${pending.user.username} cannot be undone. It is refused while the account still owns accounts (or, for an agent, still has users).`,
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

/** 生成一个好读好抄的初始密码：12 位，去掉 0/O、1/l/I 这类容易看错的字符。 */
function generatePassword(): string {
  const alphabet = 'abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'
  const bytes = new Uint32Array(12)
  crypto.getRandomValues(bytes)
  return Array.from(bytes, (b) => alphabet[b % alphabet.length]).join('')
}

/** 一枚复制按钮：点了换成对勾，两秒后复原。 */
function CopyButton({ text, label }: { text: string; label: string }) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  return (
    <Hint label={copied ? t('已复制', 'Copied') : label}>
      <Button
        aria-label={label}
        size="icon-xs"
        type="button"
        variant="ghost"
        onClick={() => {
          void copyText(text).then((ok) => {
            if (!ok) return
            setCopied(true)
            setTimeout(() => setCopied(false), 2000)
          })
        }}
      >
        {copied ? <CheckIcon /> : <CopyIcon />}
      </Button>
    </Hint>
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
      description: t('上号并查看自己号的用量。', 'Adds accounts and sees their own usage.'),
    },
    {
      value: 'agent' as const,
      title: t('代理', 'Agent'),
      description: t('上号，并可开设挂在自己名下的用户。', 'Adds accounts and can create users under them.'),
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
              <DialogTitle>{t('账号已创建', 'Account created')}</DialogTitle>
              <DialogDescription>
                {t(
                  '请把以下登录信息交给对方。关闭后将无法再查看密码，忘记时只能重置。',
                  'Hand these sign-in details over. The password cannot be shown again after closing; it can only be reset.',
                )}
              </DialogDescription>
            </DialogHeader>
            <DialogPanel>
              <SignInDetails password={created.password} username={created.user.username} />
            </DialogPanel>
            <DialogFooter>
              <CopyButtonWide text={signInText(created.user.username, created.password, t)} />
              <DialogClose render={<Button />}>{t('完成', 'Done')}</DialogClose>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>{isAdmin ? t('新建账号', 'New account') : t('新建用户', 'New user')}</DialogTitle>
              <DialogDescription>
                {isAdmin
                  ? t('对方用这里设置的用户名和密码登录控制台。', 'They sign in to the console with this username and password.')
                  : t('新用户挂在你名下，用这里设置的用户名和密码登录控制台。', 'The new user belongs to you and signs in with this username and password.')}
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
                    <div aria-label={t('角色', 'Role')} className="grid w-full gap-2 sm:grid-cols-2" role="radiogroup">
                      {roleOptions.map((option) => {
                        const selected = role === option.value
                        return (
                          <button
                            aria-checked={selected}
                            className={cn(
                              'flex cursor-pointer flex-col items-start gap-1 rounded-lg border px-3 py-2.5 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
                              selected ? 'border-primary bg-primary/6' : 'hover:bg-accent/60',
                            )}
                            key={option.value}
                            role="radio"
                            type="button"
                            onClick={() => setRole(option.value)}
                          >
                            <span className="flex w-full items-center justify-between text-sm font-medium">
                              {option.title}
                              {selected && <CheckIcon aria-hidden="true" className="size-4 text-primary" />}
                            </span>
                            <span className="text-xs text-muted-foreground">{option.description}</span>
                          </button>
                        )
                      })}
                    </div>
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
                      {t('挂在代理名下时，由该代理管理这个用户。', 'When placed under an agent, that agent manages this user.')}
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
                  <PasswordInput invalid={passwordTooShort} value={password} onChange={setPassword} />
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

/** 初始 / 新密码的输入框：可显示明文，可一键随机生成（生成后自动显示，方便核对）。 */
function PasswordInput({
  value,
  onChange,
  autoFocus,
  invalid,
}: {
  value: string
  onChange: (value: string) => void
  autoFocus?: boolean
  invalid?: boolean
}) {
  const { t } = useI18n()
  const [show, setShow] = useState(false)
  return (
    <InputGroup>
      <InputGroupInput
        aria-invalid={invalid || undefined}
        autoComplete="new-password"
        autoFocus={autoFocus}
        onChange={(event) => onChange(event.target.value)}
        type={show ? 'text' : 'password'}
        value={value}
      />
      <InputGroupAddon align="inline-end">
        <Hint label={show ? t('隐藏密码', 'Hide password') : t('显示密码', 'Show password')}>
          <Button
            aria-label={show ? t('隐藏密码', 'Hide password') : t('显示密码', 'Show password')}
            size="icon-xs"
            type="button"
            variant="ghost"
            onClick={() => setShow((v) => !v)}
          >
            {show ? <EyeOffIcon /> : <EyeIcon />}
          </Button>
        </Hint>
        <Hint label={t('随机生成', 'Generate')}>
          <Button
            aria-label={t('随机生成密码', 'Generate a password')}
            size="icon-xs"
            type="button"
            variant="ghost"
            onClick={() => { onChange(generatePassword()); setShow(true) }}
          >
            <DicesIcon />
          </Button>
        </Hint>
      </InputGroupAddon>
    </InputGroup>
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

/** 一次复制整份登录信息的按钮。 */
function CopyButtonWide({ text }: { text: string }) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  return (
    <Button
      type="button"
      variant="outline"
      onClick={() => {
        void copyText(text).then((ok) => {
          if (!ok) return
          setCopied(true)
          setTimeout(() => setCopied(false), 2000)
        })
      }}
    >
      {copied ? <CheckIcon /> : <CopyIcon />}
      {copied ? t('已复制', 'Copied') : t('复制全部', 'Copy all')}
    </Button>
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
                  '该账号已有的登录已全部退出。请把新的登录信息交给对方，关闭后将无法再查看密码。',
                  'All existing sign-ins of this account were signed out. Hand over the new details; the password cannot be shown again after closing.',
                )}
              </DialogDescription>
            </DialogHeader>
            <DialogPanel>
              <SignInDetails password={reset} username={user.username} />
            </DialogPanel>
            <DialogFooter>
              <CopyButtonWide text={signInText(user.username, reset, t)} />
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
                  <PasswordInput autoFocus invalid={tooShort} value={password} onChange={setPassword} />
                  <FieldDescription>
                    {t(
                      `至少 ${MIN_PASSWORD_LENGTH} 个字符。重置后，该账号已有的登录全部退出。`,
                      `At least ${MIN_PASSWORD_LENGTH} characters. All existing sign-ins of this account are signed out.`,
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
              {t('转移后，新上级可以管理这个用户；号仍归用户本人。', 'The new parent manages this user afterwards; the accounts stay with the user.')}
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

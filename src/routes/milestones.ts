import { Router, Request, Response, NextFunction } from 'express'
import { authenticate } from '../middleware/auth.js'
import { requireUser, requireVerifier } from '../middleware/rbac.js'
import {
  createMilestone,
  getMilestonesByVaultId,
  getMilestoneById,
  verifyMilestone,
  validateMilestone,
  allMilestonesVerified,
} from '../services/milestones.js'
import {
  recordMilestoneApproval,
  hasVerifierVoted,
  getMilestoneApprovalProgress,
  hasMilestoneMetThreshold,
  DuplicateVerifierVoteError,
} from '../services/verifiers.js'
import { completeVault } from '../services/vaultTransitions.js'
import { vaults } from './vaults.js'
import { getVaultById } from '../services/vaultStore.js'
import { AppError } from '../middleware/errorHandler.js'

export const milestonesRouter = Router({ mergeParams: true })

// POST /api/vaults/:vaultId/milestones
milestonesRouter.post('/', authenticate, requireUser, (req: Request, res: Response, next: NextFunction) => {
  const { vaultId } = req.params
  const vault = vaults.find((v) => v.id === vaultId)

  if (!vault) {
    return next(AppError.notFound('Vault not found'))
  }

  if (vault.status !== 'active') {
    return next(AppError.conflict('Cannot add milestones to a non-active vault'))
  }

  const { description } = req.body as { description?: string }
  if (!description?.trim()) {
    return next(AppError.badRequest('description is required'))
  }

  const milestone = createMilestone(vaultId, description.trim(), vault.verifier)
  res.status(201).json(milestone)
})

// GET /api/vaults/:vaultId/milestones
milestonesRouter.get('/', (req: Request, res: Response, next: NextFunction) => {
  const { vaultId } = req.params
  const vault = vaults.find((v) => v.id === vaultId)

  if (!vault) {
    return next(AppError.notFound('Vault not found'))
  }

  const milestones = getMilestonesByVaultId(vaultId)
  res.json({ milestones })
})

// PATCH /api/vaults/:vaultId/milestones/:id/verify
milestonesRouter.patch('/:id/verify', authenticate, requireVerifier, (req: Request, res: Response, next: NextFunction) => {
  const { vaultId, id } = req.params

  const vault = vaults.find((v) => v.id === vaultId)
  if (!vault) {
    return next(AppError.notFound('Vault not found'))
  }

  const milestone = getMilestoneById(id)
  if (!milestone || milestone.vaultId !== vaultId) {
    return next(AppError.notFound('Milestone not found'))
  }

  const verified = verifyMilestone(id)
  if (!verified) {
    return next(AppError.notFound('Milestone not found'))
  }

  let vaultCompleted = false
  if (allMilestonesVerified(vaultId) && vault.status === 'active') {
    const result = completeVault(vaultId)
    vaultCompleted = result.success
  }

  res.json({ milestone: verified, vaultCompleted })
})

// POST /api/vaults/:vaultId/milestones/:id/validate
milestonesRouter.post('/:id/validate', authenticate, requireVerifier, async (req: Request, res: Response, next: NextFunction) => {
  const { vaultId, id } = req.params
  const validatorUserId = req.user!.userId

  // Prefer DB-backed vault (has lateCheckInWindowSecs + PersistedMilestone.dueDate)
  const persistedVault = await getVaultById(vaultId).catch(() => null)
  const vault = persistedVault ?? vaults.find((v) => v.id === vaultId)

  if (!vault) {
    return next(AppError.notFound('Vault not found'))
  }

  const milestone = getMilestoneById(id)
  if (!milestone || milestone.vaultId !== vaultId) {
    return next(AppError.notFound('Milestone not found'))
  }

  // ── Deadline + grace window enforcement ──────────────────────────────────
  // Resolve dueDate from the in-memory milestone or from the persisted vault's
  // milestone list (which carries dueDate from the DB).
  const persistedMilestone = persistedVault?.milestones.find((m) => m.id === id)
  const dueDate = milestone.dueDate ?? persistedMilestone?.dueDate ?? null

  if (dueDate) {
    const now = Date.now()
    const dueDateMs = Date.parse(dueDate)
    const endDateMs = Date.parse(vault.endDate ?? vault.endTimestamp ?? '')
    const graceWindowMs = (persistedVault?.lateCheckInWindowSecs ?? (vault as any).lateCheckInWindowSecs ?? 0) * 1000

    // Effective deadline: dueDate + grace window, but never past vault endDate
    const effectiveDeadlineMs = Number.isFinite(endDateMs)
      ? Math.min(dueDateMs + graceWindowMs, endDateMs)
      : dueDateMs + graceWindowMs

    if (now > effectiveDeadlineMs) {
      return next(AppError.badRequest('DeadlinePassed: check-in window has closed for this milestone'))
    }
  }
  // ─────────────────────────────────────────────────────────────────────────

  const result = validateMilestone(id, validatorUserId)
  if (!result.success) {
    if (result.error === 'Milestone already validated') {
      return next(AppError.conflict('Milestone already validated'))
    }
    if (result.error === 'Unauthorized: only assigned verifier can validate') {
      return next(AppError.forbidden('Unauthorized: only assigned verifier can validate'))
    }
    return next(AppError.badRequest(result.error!))
  }

  let vaultCompleted = false
  if (allMilestonesVerified(vaultId) && vault.status === 'active') {
    const result = completeVault(vaultId)
    vaultCompleted = result.success
  }

  res.json({ milestone: result.milestone, vaultCompleted })
})

// POST /api/vaults/:vaultId/milestones/:id/approve
// Multi-verifier approval endpoint with duplicate-vote prevention
milestonesRouter.post('/:id/approve', authenticate, requireVerifier, async (req: Request, res: Response, next: NextFunction) => {
  try {
    const { vaultId, id } = req.params
    const verifierUserId = req.user!.userId
    const { approvalStatus } = req.body as { approvalStatus?: string }

    // Validate input
    if (!approvalStatus || !['approved', 'rejected'].includes(approvalStatus)) {
      return next(AppError.badRequest('approvalStatus must be "approved" or "rejected"'))
    }

    // Check vault exists
    const vault = vaults.find((v) => v.id === vaultId)
    if (!vault) {
      return next(AppError.notFound('Vault not found'))
    }

    // Check milestone exists and belongs to vault
    const milestone = getMilestoneById(id)
    if (!milestone || milestone.vaultId !== vaultId) {
      return next(AppError.notFound('Milestone not found'))
    }

    // Check if verifier has already voted (duplicate vote prevention)
    const hasVoted = await hasVerifierVoted(id, verifierUserId)
    if (hasVoted) {
      return next(AppError.conflict('Verifier has already voted on this milestone'))
    }

    // Record the approval
    const approval = await recordMilestoneApproval(id, verifierUserId, approvalStatus as any)

    // Get updated approval progress
    const milestone_record = getMilestoneById(id)
    const approvalThreshold = (milestone_record as any)?.approvalThreshold || 1
    const approvalProgress = await getMilestoneApprovalProgress(id, approvalThreshold)

    // Check if milestone should be completed (if all approvals met and no rejections)
    let milestoneCompleted = false
    let vaultCompleted = false

    if (approvalProgress.isComplete && !approvalProgress.isRejected) {
      milestoneCompleted = true
      milestone.verified = true
      milestone.verifiedAt = new Date().toISOString()
      milestone.verifiedBy = verifierUserId

      // Check if all vault milestones are now verified
      if (allMilestonesVerified(vaultId) && vault.status === 'active') {
        const result = completeVault(vaultId)
        vaultCompleted = result.success
      }
    }

    res.status(201).json({
      approval,
      approvalProgress,
      milestone: {
        ...milestone,
        approvalThreshold,
      },
      milestoneCompleted,
      vaultCompleted,
    })
  } catch (error) {
    if (error instanceof DuplicateVerifierVoteError) {
      return next(AppError.conflict(error.message))
    }
    next(error)
  }
})

// GET /api/vaults/:vaultId/milestones/:id/approval-status
// Get detailed approval status for a milestone
milestonesRouter.get('/:id/approval-status', async (req: Request, res: Response, next: NextFunction) => {
  try {
    const { vaultId, id } = req.params

    // Check vault exists
    const vault = vaults.find((v) => v.id === vaultId)
    if (!vault) {
      return next(AppError.notFound('Vault not found'))
    }

    // Check milestone exists
    const milestone = getMilestoneById(id)
    if (!milestone || milestone.vaultId !== vaultId) {
      return next(AppError.notFound('Milestone not found'))
    }

    const approvalThreshold = (milestone as any)?.approvalThreshold || 1
    const approvalProgress = await getMilestoneApprovalProgress(id, approvalThreshold)

    res.json({
      milestone: {
        id: milestone.id,
        vaultId: milestone.vaultId,
        description: milestone.description,
        approvalThreshold,
      },
      approvalStatus: approvalProgress,
    })
  } catch (error) {
    next(error)
  }
})
